//! Typed, paged readers over a validated `.lrcat` (see [`crate::open`]). Everything is read by its
//! *declared* column type -- `rating`/`pick` are `REAL`, and a naive numeric op on a bitmask-string
//! column silently coerces instead of erroring (ADR-0061 Q2). Images are paged by `id_local`
//! keyset so a chunked import job never holds a statement across `step()` calls and never loads a
//! 380k-row catalog into memory.

use rusqlite::{params, Connection};

use crate::open::table_columns;
use crate::paths::rel_path_under_root;
use crate::StrayError;

/// One `AgLibraryRootFolder` row.
#[derive(Debug, Clone, PartialEq)]
pub struct LrcRoot {
    pub id: i64,
    pub name: Option<String>,
    /// Drive-letter path as LRC recorded it (`D:\Photos\`); every real root has one (ADR-0061 Q1).
    pub absolute_path: String,
}

/// The current develop-settings row of an image.
#[derive(Debug, Clone, PartialEq)]
pub struct LrcDevelop {
    /// The verbatim `s = { Key = Value, ... }` Lua literal.
    pub text: String,
    pub process_version: Option<String>,
    pub has_masks: Option<bool>,
    pub has_ai_masks: Option<bool>,
    pub has_big_data: Option<bool>,
}

/// One `Adobe_images` row joined to its file/folder (and, when asked for, IPTC + develop settings).
#[derive(Debug, Clone, PartialEq)]
pub struct LrcImage {
    pub id_local: i64,
    /// `Adobe_images.id_global`, or `local:<id_local>` for a catalog without that column.
    pub id_global: String,
    pub root_id: i64,
    /// Path under the root folder in the form `scruff` stores it (forward slashes, NFC).
    pub rel_path: String,
    /// `NULL` = unrated (277k of 380k real rows), *not* zero stars.
    pub rating: Option<f64>,
    /// `1` = flagged pick, `0` = none, `-1` = rejected.
    pub pick: Option<f64>,
    pub color_label: Option<String>,
    /// Set for a virtual copy: the master image's `id_local`.
    pub master_image: Option<i64>,
    pub copy_name: Option<String>,
    pub file_width: Option<i64>,
    pub file_height: Option<i64>,
    /// `Adobe_images.orientation` (`AB` = upright); `None` if the column is absent.
    pub orientation: Option<String>,
    pub import_hash: Option<String>,
    pub iptc_caption: Option<String>,
    pub iptc_copyright: Option<String>,
    pub develop: Option<LrcDevelop>,
}

/// One keyword node with its resolved name chain (root-most first), skipping LRC's unnamed root.
#[derive(Debug, Clone, PartialEq)]
pub struct LrcKeyword {
    pub id: i64,
    pub path: Vec<String>,
}

/// One collection or collection set. Only user collections/sets are returned -- never the
/// `*.unsaved` per-module scratch rows (ADR-0061 Q3).
#[derive(Debug, Clone, PartialEq)]
pub struct LrcCollection {
    pub id: i64,
    pub name: String,
    pub parent: Option<i64>,
    pub kind: CollectionKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollectionKind {
    /// `com.adobe.ag.library.collection`.
    Manual,
    /// `com.adobe.ag.library.group`: a collection *set*, imported as an empty parent collection.
    Set,
    /// `com.adobe.ag.library.smart_collection`: counted, not imported (rule mapping unverified).
    Smart,
}

/// Which optional columns/tables this catalog has, resolved once so the page query is exact.
#[derive(Debug, Clone, Default)]
struct Shape {
    id_global: bool,
    master_image: bool,
    copy_name: bool,
    orientation: bool,
    file_dims: bool,
    import_hash: bool,
    iptc: bool,
    develop: bool,
    develop_process_version: bool,
    develop_has_masks: bool,
    develop_has_ai_masks: bool,
    develop_has_big_data: bool,
    position_in_collection: bool,
}

/// Reader over one open catalog.
pub struct Reader<'c> {
    conn: &'c Connection,
    shape: Shape,
}

/// SQL predicate selecting images in these root folders (ids are `i64`, so inlined safely); empty
/// selects everything.
fn root_filter(roots: &[i64]) -> String {
    if roots.is_empty() {
        "1".to_string()
    } else {
        let list: Vec<String> = roots.iter().map(i64::to_string).collect();
        format!("fo.rootFolder IN ({})", list.join(","))
    }
}

fn flag(v: Option<f64>) -> Option<bool> {
    v.map(|n| n != 0.0)
}

/// A numeric column read by its *stored* value, not its name: the real catalog stores
/// `fileWidth`/`fileHeight` (and others a schema dump calls INTEGER) as `REAL`, which `i64`
/// refuses -- found only by running against a real v13 backup.
fn num(row: &rusqlite::Row<'_>, idx: usize) -> rusqlite::Result<Option<f64>> {
    use rusqlite::types::ValueRef;
    Ok(match row.get_ref(idx)? {
        ValueRef::Null => None,
        ValueRef::Integer(i) => Some(i as f64),
        ValueRef::Real(f) => Some(f),
        ValueRef::Text(t) => std::str::from_utf8(t)
            .ok()
            .and_then(|s| s.trim().parse().ok()),
        ValueRef::Blob(_) => None,
    })
}

/// A text column that a catalog may have stored as a number (`processVersion` is `"15.4"` in every
/// real row measured, but nothing guarantees it), rendered the way LRC prints it.
fn text_col(row: &rusqlite::Row<'_>, idx: usize) -> rusqlite::Result<Option<String>> {
    use rusqlite::types::ValueRef;
    Ok(match row.get_ref(idx)? {
        ValueRef::Null | ValueRef::Blob(_) => None,
        ValueRef::Text(t) => Some(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Integer(i) => Some(format!("{i}.0")),
        ValueRef::Real(f) => Some(format!("{f:.1}")),
    })
}

fn whole(row: &rusqlite::Row<'_>, idx: usize) -> rusqlite::Result<Option<i64>> {
    Ok(num(row, idx)?.map(|n| n.round() as i64))
}

impl<'c> Reader<'c> {
    pub fn new(conn: &'c Connection) -> Result<Self, StrayError> {
        let has = |table: &str, col: &str| -> Result<bool, StrayError> {
            Ok(table_columns(conn, table)?.iter().any(|c| c == col))
        };
        let iptc_cols = table_columns(conn, "AgLibraryIPTC")?;
        let dev_cols = table_columns(conn, "Adobe_imageDevelopSettings")?;
        let shape = Shape {
            id_global: has("Adobe_images", "id_global")?,
            master_image: has("Adobe_images", "masterImage")?,
            copy_name: has("Adobe_images", "copyName")?,
            orientation: has("Adobe_images", "orientation")?,
            file_dims: has("Adobe_images", "fileWidth")? && has("Adobe_images", "fileHeight")?,
            import_hash: has("AgLibraryFile", "importHash")?,
            iptc: ["image", "caption", "copyright"]
                .iter()
                .all(|c| iptc_cols.iter().any(|x| x == c)),
            develop: ["image", "text"]
                .iter()
                .all(|c| dev_cols.iter().any(|x| x == c)),
            develop_process_version: dev_cols.iter().any(|c| c == "processVersion"),
            develop_has_masks: dev_cols.iter().any(|c| c == "hasMasks"),
            develop_has_ai_masks: dev_cols.iter().any(|c| c == "hasAIMasks"),
            develop_has_big_data: dev_cols.iter().any(|c| c == "hasBigData"),
            position_in_collection: has("AgLibraryCollectionImage", "positionInCollection")?,
        };
        Ok(Reader { conn, shape })
    }

    /// Whether develop settings can be read at all (a catalog without the table imports metadata
    /// only).
    pub fn has_develop(&self) -> bool {
        self.shape.develop
    }

    pub fn roots(&self) -> Result<Vec<LrcRoot>, StrayError> {
        let has_name = table_columns(self.conn, "AgLibraryRootFolder")?
            .iter()
            .any(|c| c == "name");
        let sql = format!(
            "SELECT id_local, {}, absolutePath FROM AgLibraryRootFolder ORDER BY id_local",
            if has_name { "name" } else { "NULL" }
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map([], |row| {
                Ok(LrcRoot {
                    id: whole(row, 0)?.unwrap_or_default(),
                    name: row.get(1)?,
                    absolute_path: row.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Images under the given root folders (empty = every root).
    pub fn image_count(&self, roots: &[i64]) -> Result<u64, StrayError> {
        let n: i64 = self.conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM Adobe_images i \
                 JOIN AgLibraryFile f ON f.id_local = i.rootFile \
                 JOIN AgLibraryFolder fo ON fo.id_local = f.folder \
                 WHERE {}",
                root_filter(roots)
            ),
            [],
            |row| row.get(0),
        )?;
        Ok(n as u64)
    }

    /// Up to `limit` images with `id_local > after`, in `id_local` order, optionally restricted to
    /// some roots (empty = all). `develop` also joins IPTC and the image's current develop-settings row (the
    /// latest, when -- as for 7 of 380k real images -- there is more than one).
    pub fn images_page(
        &self,
        roots: &[i64],
        after: i64,
        limit: usize,
        develop: bool,
    ) -> Result<Vec<LrcImage>, StrayError> {
        let s = &self.shape;
        let opt = |present: bool, expr: &str| {
            if present {
                expr.to_string()
            } else {
                "NULL".to_string()
            }
        };
        let with_dev = develop && s.develop;
        let with_iptc = develop && s.iptc;
        let sql = format!(
            "SELECT i.id_local, {id_global}, fo.rootFolder, fo.pathFromRoot, f.baseName, \
                    f.extension, i.rating, i.pick, i.colorLabels, {master}, {copy}, {fw}, {fh}, \
                    {hash}, {cap}, {copyr}, {dtext}, {dpv}, {dm}, {dai}, {dbd}, {orient} \
             FROM Adobe_images i \
             JOIN AgLibraryFile f ON f.id_local = i.rootFile \
             JOIN AgLibraryFolder fo ON fo.id_local = f.folder \
             {iptc_join} {dev_join} \
             WHERE i.id_local > ?1 AND {roots} \
             ORDER BY i.id_local LIMIT ?2",
            roots = root_filter(roots),
            id_global = if s.id_global {
                "i.id_global".to_string()
            } else {
                "'local:' || CAST(i.id_local AS TEXT)".to_string()
            },
            master = opt(s.master_image, "i.masterImage"),
            copy = opt(s.copy_name, "i.copyName"),
            fw = opt(s.file_dims, "i.fileWidth"),
            fh = opt(s.file_dims, "i.fileHeight"),
            hash = opt(s.import_hash, "f.importHash"),
            orient = opt(s.orientation, "i.orientation"),
            cap = opt(with_iptc, "p.caption"),
            copyr = opt(with_iptc, "p.copyright"),
            dtext = opt(with_dev, "d.text"),
            dpv = opt(with_dev && s.develop_process_version, "d.processVersion"),
            dm = opt(with_dev && s.develop_has_masks, "d.hasMasks"),
            dai = opt(with_dev && s.develop_has_ai_masks, "d.hasAIMasks"),
            dbd = opt(with_dev && s.develop_has_big_data, "d.hasBigData"),
            iptc_join = if with_iptc {
                "LEFT JOIN AgLibraryIPTC p ON p.image = i.id_local"
            } else {
                ""
            },
            dev_join = if with_dev {
                "LEFT JOIN Adobe_imageDevelopSettings d ON d.id_local = \
                 (SELECT MAX(id_local) FROM Adobe_imageDevelopSettings WHERE image = i.id_local)"
            } else {
                ""
            },
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![after, limit as i64], |row| {
                let folder: Option<String> = row.get(3)?;
                let base: Option<String> = row.get(4)?;
                let ext: Option<String> = row.get(5)?;
                let dev_text: Option<String> = row.get(16)?;
                let develop = dev_text.map(|text| LrcDevelop {
                    text,
                    process_version: text_col(row, 17).ok().flatten(),
                    has_masks: flag(num(row, 18).ok().flatten()),
                    has_ai_masks: flag(num(row, 19).ok().flatten()),
                    has_big_data: flag(num(row, 20).ok().flatten()),
                });
                Ok(LrcImage {
                    id_local: whole(row, 0)?.unwrap_or_default(),
                    id_global: row.get(1)?,
                    root_id: whole(row, 2)?.unwrap_or_default(),
                    rel_path: rel_path_under_root(
                        folder.as_deref().unwrap_or(""),
                        base.as_deref().unwrap_or(""),
                        ext.as_deref().unwrap_or(""),
                    ),
                    rating: num(row, 6)?,
                    pick: num(row, 7)?,
                    color_label: row.get(8)?,
                    master_image: whole(row, 9)?,
                    copy_name: row.get(10)?,
                    file_width: whole(row, 11)?,
                    file_height: whole(row, 12)?,
                    import_hash: row.get(13)?,
                    iptc_caption: row.get(14)?,
                    iptc_copyright: row.get(15)?,
                    orientation: row.get(21)?,
                    develop,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Every keyword with its resolved name path. `genealogy` is a `/`-separated chain of ancestor
    /// ids including the keyword's own (ADR-0061 research); LRC's single unnamed root (NULL/empty
    /// name) is skipped as a segment. A keyword whose chain resolves to no names is dropped.
    pub fn keywords(&self) -> Result<Vec<LrcKeyword>, StrayError> {
        let mut stmt = self
            .conn
            .prepare("SELECT id_local, name, genealogy FROM AgLibraryKeyword ORDER BY id_local")?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    whole(row, 0)?.unwrap_or_default(),
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let names: std::collections::HashMap<i64, String> = rows
            .iter()
            .filter_map(|(id, name, _)| {
                name.as_ref()
                    .filter(|n| !n.trim().is_empty())
                    .map(|n| (*id, n.clone()))
            })
            .collect();
        let mut out = Vec::new();
        for (id, name, genealogy) in &rows {
            if name.as_deref().is_none_or(|n| n.trim().is_empty()) {
                continue;
            }
            let mut path: Vec<String> = genealogy
                .as_deref()
                .unwrap_or("")
                .split('/')
                .filter_map(|seg| seg.trim().parse::<i64>().ok())
                .filter_map(|ancestor| names.get(&ancestor).cloned())
                .collect();
            // A catalog whose genealogy omits the keyword's own id still names it last.
            if path.last() != name.as_ref() {
                path.push(name.clone().unwrap_or_default());
            }
            if !path.is_empty() {
                out.push(LrcKeyword { id: *id, path });
            }
        }
        Ok(out)
    }

    /// `Adobe_images.id_local` of every image tagged with keyword `tag`.
    pub fn keyword_images(&self, tag: i64) -> Result<Vec<i64>, StrayError> {
        let mut stmt = self
            .conn
            .prepare("SELECT image FROM AgLibraryKeywordImage WHERE tag = ?1")?;
        let ids = stmt
            .query_map(params![tag], |row| Ok(whole(row, 0)?.unwrap_or_default()))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    /// User collections and collection sets (parents first is *not* guaranteed -- callers resolve
    /// `parent` after creating everything), plus a count of smart collections skipped.
    pub fn collections(&self) -> Result<(Vec<LrcCollection>, u64), StrayError> {
        let mut stmt = self.conn.prepare(
            "SELECT id_local, name, parent, creationId FROM AgLibraryCollection ORDER BY id_local",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    whole(row, 0)?.unwrap_or_default(),
                    row.get::<_, Option<String>>(1)?,
                    whole(row, 2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut out = Vec::new();
        let mut smart = 0;
        for (id, name, parent, creation) in rows {
            let kind = match creation.as_deref() {
                Some("com.adobe.ag.library.collection") => CollectionKind::Manual,
                Some("com.adobe.ag.library.group") => CollectionKind::Set,
                Some("com.adobe.ag.library.smart_collection") => {
                    smart += 1;
                    continue;
                }
                // `*.unsaved` module scratch state, and anything this importer doesn't know.
                _ => continue,
            };
            out.push(LrcCollection {
                id,
                name: name.unwrap_or_default(),
                parent,
                kind,
            });
        }
        Ok((out, smart))
    }

    /// `Adobe_images.id_local` of a collection's members, in LRC's own order.
    pub fn collection_images(&self, collection: i64) -> Result<Vec<i64>, StrayError> {
        let order = if self.shape.position_in_collection {
            "ORDER BY positionInCollection, image"
        } else {
            "ORDER BY image"
        };
        let mut stmt = self.conn.prepare(&format!(
            "SELECT image FROM AgLibraryCollectionImage WHERE collection = ?1 {order}"
        ))?;
        let ids = stmt
            .query_map(params![collection], |row| {
                Ok(whole(row, 0)?.unwrap_or_default())
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ids)
    }
}
