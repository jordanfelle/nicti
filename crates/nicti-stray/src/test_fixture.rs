//! Builds a synthetic `.lrcat`-shaped SQLite database for tests: the table/column subset the
//! importer reads, with the real v13 names (ADR-0061, `docs/research/shed-lrcat-schema.md`) and
//! only invented values -- never a real keyword, collection or path string (`spikes/shed`'s
//! privacy rule). Shared by this crate's unit tests and, via `#[path]`, its integration tests, so
//! it must not use `crate::` paths.
#![allow(dead_code)]

use rusqlite::{params, Connection};

pub fn create_schema(conn: &Connection) {
    conn.execute_batch(
        r#"
        CREATE TABLE AgLibraryRootFolder (
            id_local INTEGER PRIMARY KEY, id_global TEXT, absolutePath TEXT, name TEXT,
            relativePathFromCatalog TEXT);
        CREATE TABLE AgLibraryFolder (
            id_local INTEGER PRIMARY KEY, id_global TEXT, parentId INTEGER, pathFromRoot TEXT,
            rootFolder INTEGER);
        CREATE TABLE AgLibraryFile (
            id_local INTEGER PRIMARY KEY, id_global TEXT, baseName TEXT, extension TEXT,
            folder INTEGER, importHash TEXT, md5 TEXT);
        CREATE TABLE Adobe_images (
            id_local INTEGER PRIMARY KEY, id_global TEXT, rootFile INTEGER, rating, pick,
            colorLabels TEXT, masterImage INTEGER, copyName TEXT, fileWidth INTEGER,
            fileHeight INTEGER, orientation TEXT);
        CREATE TABLE Adobe_imageDevelopSettings (
            id_local INTEGER PRIMARY KEY, image INTEGER, text TEXT, processVersion TEXT,
            hasMasks INTEGER, hasAIMasks INTEGER, hasBigData INTEGER);
        CREATE TABLE AgLibraryKeyword (
            id_local INTEGER PRIMARY KEY, name TEXT, genealogy TEXT, parent INTEGER);
        CREATE TABLE AgLibraryKeywordImage (id_local INTEGER PRIMARY KEY, image INTEGER, tag INTEGER);
        CREATE TABLE AgLibraryCollection (
            id_local INTEGER PRIMARY KEY, name TEXT, parent INTEGER, creationId TEXT);
        CREATE TABLE AgLibraryCollectionImage (
            id_local INTEGER PRIMARY KEY, collection INTEGER, image INTEGER,
            positionInCollection REAL);
        CREATE TABLE AgLibraryIPTC (
            id_local INTEGER PRIMARY KEY, image INTEGER, caption TEXT, copyright TEXT);
        "#,
    )
    .unwrap();
}

pub fn add_root(conn: &Connection, id: i64, absolute_path: &str) {
    conn.execute(
        "INSERT INTO AgLibraryRootFolder (id_local, absolutePath, name) VALUES (?1, ?2, ?3)",
        params![id, absolute_path, format!("root{id}")],
    )
    .unwrap();
}

pub fn add_folder(conn: &Connection, id: i64, root: i64, path_from_root: &str) {
    conn.execute(
        "INSERT INTO AgLibraryFolder (id_local, rootFolder, pathFromRoot) VALUES (?1, ?2, ?3)",
        params![id, root, path_from_root],
    )
    .unwrap();
}

pub fn add_file(conn: &Connection, id: i64, folder: i64, base: &str, ext: &str) {
    conn.execute(
        "INSERT INTO AgLibraryFile (id_local, baseName, extension, folder, importHash) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![id, base, ext, folder, format!("hash{id}")],
    )
    .unwrap();
}

pub struct Image<'a> {
    pub id: i64,
    pub file: i64,
    pub rating: Option<f64>,
    pub pick: f64,
    pub label: &'a str,
    pub master: Option<i64>,
    pub copy_name: Option<&'a str>,
}

impl Image<'_> {
    pub fn plain(id: i64, file: i64) -> Image<'static> {
        Image {
            id,
            file,
            rating: None,
            pick: 0.0,
            label: "",
            master: None,
            copy_name: None,
        }
    }
}

pub fn add_image(conn: &Connection, img: &Image) {
    conn.execute(
        "INSERT INTO Adobe_images (id_local, id_global, rootFile, rating, pick, colorLabels, \
         masterImage, copyName, fileWidth, fileHeight, orientation) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 6000, 4000, 'AB')",
        params![
            img.id,
            format!("G{}", img.id),
            img.file,
            img.rating,
            img.pick,
            img.label,
            img.master,
            img.copy_name
        ],
    )
    .unwrap();
}

pub fn add_develop(conn: &Connection, image: i64, text: &str) {
    conn.execute(
        "INSERT INTO Adobe_imageDevelopSettings (image, text, processVersion, hasMasks, \
         hasAIMasks, hasBigData) VALUES (?1, ?2, '15.4', 0, 0, 0)",
        params![image, text],
    )
    .unwrap();
}

pub fn add_keyword(conn: &Connection, id: i64, name: Option<&str>, genealogy: &str) {
    conn.execute(
        "INSERT INTO AgLibraryKeyword (id_local, name, genealogy) VALUES (?1, ?2, ?3)",
        params![id, name, genealogy],
    )
    .unwrap();
}

pub fn tag(conn: &Connection, image: i64, keyword: i64) {
    conn.execute(
        "INSERT INTO AgLibraryKeywordImage (image, tag) VALUES (?1, ?2)",
        params![image, keyword],
    )
    .unwrap();
}

pub fn add_collection(conn: &Connection, id: i64, name: &str, parent: Option<i64>, creation: &str) {
    conn.execute(
        "INSERT INTO AgLibraryCollection (id_local, name, parent, creationId) VALUES (?1, ?2, ?3, ?4)",
        params![id, name, parent, creation],
    )
    .unwrap();
}

pub fn add_to_collection(conn: &Connection, collection: i64, image: i64, position: f64) {
    conn.execute(
        "INSERT INTO AgLibraryCollectionImage (collection, image, positionInCollection) \
         VALUES (?1, ?2, ?3)",
        params![collection, image, position],
    )
    .unwrap();
}

pub fn add_iptc(conn: &Connection, image: i64, caption: &str, copyright: &str) {
    conn.execute(
        "INSERT INTO AgLibraryIPTC (image, caption, copyright) VALUES (?1, ?2, ?3)",
        params![image, caption, copyright],
    )
    .unwrap();
}
