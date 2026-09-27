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
    /// #157: presence/active-usage counts for the 6 develop-setting keys `Develop` classifies as
    /// unowned (counts only -- no value contents, some can carry user preset identifiers).
    DevelopUsage { catalog: PathBuf },
    /// Fail if any of `files` contains a keyword/collection/path string pulled from `catalog`.
    PrivacyCheck {
        catalog: PathBuf,
        /// `required = true`: a bare `Vec<PathBuf>` positional accepts zero args, which would
        /// silently report "clean (0 file(s) checked)" and exit 0 on a caller mistake (e.g. an
        /// empty glob expansion) instead of failing loudly on an incomplete check.
        #[arg(required = true)]
        files: Vec<PathBuf>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Schema { catalog } => cmd_schema(&catalog),
        Command::Inventory { catalog } => cmd_inventory(&catalog),
        Command::Develop { catalog } => cmd_develop(&catalog),
        Command::DevelopUsage { catalog } => cmd_develop_usage(&catalog),
        Command::PrivacyCheck { catalog, files } => cmd_privacy_check(&catalog, &files),
    }
}

/// Quotes `name` as a SQLite identifier, doubling any embedded `"` per SQLite's own escaping rule
/// -- `sqlite_master.name` is trusted in the sense that it's schema metadata, not user input, but
/// `shed` is a generic `.lrcat` reader (any file the caller points it at, not hardcoded to one
/// catalog), and an unescaped `format!("...\"{table}\"...")` would let a table name containing a
/// `"` break out of the quoted identifier and inject arbitrary SQL into the same statement.
fn quote_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn cmd_schema(catalog: &std::path::Path) -> Result<()> {
    let conn = open::open_backup(catalog)?;
    let mut stmt =
        conn.prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")?;
    let tables: Vec<String> = stmt
        .query_map([], |r| r.get(0))?
        .collect::<Result<Vec<_>, _>>()?;
    for table in tables {
        let quoted = quote_identifier(&table);
        let count: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {quoted}"), [], |r| r.get(0))
            .with_context(|| format!("counting {table}"))?;
        let mut col_stmt = conn.prepare(&format!("PRAGMA table_info({quoted})"))?;
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

fn cmd_develop_usage(catalog: &std::path::Path) -> Result<()> {
    let conn = open::open_backup(catalog)?;
    let usage = develop::analyze_unowned_keys(&conn)?;
    println!("{}", serde_json::to_string_pretty(&usage)?);
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// Regression test: a bare `Vec<PathBuf>` positional accepts zero arguments under clap's
    /// derive defaults, which would let `shed privacy-check catalog.lrcat` (files omitted, e.g.
    /// via an empty glob expansion in a calling script) silently report "clean" having checked
    /// nothing at all, rather than failing to parse.
    #[test]
    fn privacy_check_requires_at_least_one_file() {
        let result = Cli::try_parse_from(["shed", "privacy-check", "catalog.lrcat"]);
        assert!(result.is_err());

        let result = Cli::try_parse_from(["shed", "privacy-check", "catalog.lrcat", "a.md"]);
        assert!(result.is_ok());
    }

    #[test]
    fn quote_identifier_wraps_a_plain_name() {
        assert_eq!(quote_identifier("Adobe_images"), "\"Adobe_images\"");
    }

    #[test]
    fn quote_identifier_escapes_an_embedded_quote() {
        // Real risk this guards: `shed` is a generic `.lrcat` reader, and a table name containing
        // a `"` would otherwise let a caller-controlled name break out of the quoted identifier
        // and inject arbitrary SQL into the same statement (`cmd_schema`'s COUNT/PRAGMA queries).
        assert_eq!(
            quote_identifier(r#"evil" ; DROP TABLE Adobe_images; --"#),
            "\"evil\"\" ; DROP TABLE Adobe_images; --\""
        );
    }
}
