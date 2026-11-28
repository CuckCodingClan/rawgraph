//! SQLite persistence: one file holding `meta`, `images`, `graph`, `folders`
//! and `history` (schema 2), plus the schema migrations.
//!
//! # Adding a schema version
//!
//! To introduce schema 3:
//! 1. raise [`SCHEMA_VERSION`] to 3 and update `SCHEMA_SQL` / the writer and
//!    reader for the new layout;
//! 2. write `fn migrate_2_to_3(conn: &Connection) -> Result<(), ProjectError>`
//!    that turns a schema-2 database into schema 3 with plain SQL (and Rust
//!    where values must be generated). It must not touch `schema_version`;
//! 3. append `(2, migrate_2_to_3)` to [`MIGRATIONS`].
//!
//! Steps run in order on an in-memory copy of the file, so a failing step
//! never damages the file on disk; the test `migration_chain_is_complete`
//! fails if a step is missing.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rawgraph_core::{CaptureInfo, Graph, ImageId};
use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, Connection, ErrorCode, OpenFlags, OptionalExtension, TransactionBehavior};

use crate::history::{HistoryEntry, HistoryLog};
use crate::{ids, ColorLabel, Flag, ImageEntry, Project, ProjectError, SourceFolder, SCHEMA_VERSION};

const SCHEMA_SQL: &str = "
CREATE TABLE meta (
    key   TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
);
CREATE TABLE images (
    position     INTEGER PRIMARY KEY NOT NULL,
    id           INTEGER NOT NULL UNIQUE,
    uuid         TEXT NOT NULL UNIQUE,
    path         TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    file_size    INTEGER NOT NULL,
    modified_unix_ms INTEGER,
    capture      TEXT NOT NULL,
    rating       INTEGER NOT NULL,
    flag         TEXT NOT NULL,
    color_label  TEXT,
    missing      INTEGER NOT NULL
);
CREATE TABLE graph (
    id   INTEGER PRIMARY KEY NOT NULL CHECK (id = 1),
    json TEXT NOT NULL
);
CREATE TABLE folders (
    position  INTEGER PRIMARY KEY NOT NULL,
    path      TEXT NOT NULL,
    recursive INTEGER NOT NULL
);
CREATE TABLE history (
    revision      INTEGER PRIMARY KEY NOT NULL,
    time_unix_ms  INTEGER NOT NULL,
    author        TEXT NOT NULL,
    device        TEXT NOT NULL,
    summary       TEXT NOT NULL,
    graph_json    TEXT
);
";

/// One migration step: changes a database from schema `from` to `from + 1`.
type Migration = fn(&Connection) -> Result<(), ProjectError>;

/// Ordered migration steps, `(from_version, step)`. See the module documentation.
const MIGRATIONS: &[(u32, Migration)] = &[(1, migrate_1_to_2)];

fn db_err(e: rusqlite::Error) -> ProjectError {
    if let rusqlite::Error::SqliteFailure(f, _) = &e {
        if matches!(f.code, ErrorCode::NotADatabase | ErrorCode::DatabaseCorrupt) {
            return ProjectError::Corrupt(format!("not a RawGraph project file ({e})"));
        }
        if matches!(f.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) {
            return ProjectError::Io(PROJECT_BUSY_TEXT.to_owned());
        }
    }
    ProjectError::Database(e.to_string())
}

/// What the user sees when another program or computer holds the project file locked.
pub(crate) const PROJECT_BUSY_TEXT: &str = "The project file is in use by another program or computer; try again.";

fn flag_to_text(f: Flag) -> &'static str {
    match f {
        Flag::None => "none",
        Flag::Pick => "pick",
        Flag::Reject => "reject",
    }
}

fn flag_from_text(s: &str) -> Result<Flag, ProjectError> {
    match s {
        "none" => Ok(Flag::None),
        "pick" => Ok(Flag::Pick),
        "reject" => Ok(Flag::Reject),
        other => Err(ProjectError::Corrupt(format!("unknown flag \"{other}\""))),
    }
}

fn label_to_text(l: ColorLabel) -> &'static str {
    match l {
        ColorLabel::Red => "red",
        ColorLabel::Yellow => "yellow",
        ColorLabel::Green => "green",
        ColorLabel::Blue => "blue",
        ColorLabel::Purple => "purple",
    }
}

fn label_from_text(s: &str) -> Result<ColorLabel, ProjectError> {
    match s {
        "red" => Ok(ColorLabel::Red),
        "yellow" => Ok(ColorLabel::Yellow),
        "green" => Ok(ColorLabel::Green),
        "blue" => Ok(ColorLabel::Blue),
        "purple" => Ok(ColorLabel::Purple),
        other => Err(ProjectError::Corrupt(format!("unknown colour label \"{other}\""))),
    }
}

fn to_i64(v: u64, what: &str) -> Result<i64, ProjectError> {
    i64::try_from(v).map_err(|_| ProjectError::Database(format!("{what} {v} is too large to store")))
}

fn to_u64(v: i64, what: &str) -> Result<u64, ProjectError> {
    u64::try_from(v).map_err(|_| ProjectError::Corrupt(format!("{what} {v} is negative")))
}

/// Revision and history that a file is written with (a commit writes the
/// new ones before they are applied to the project).
pub(crate) struct Versioning<'a> {
    pub(crate) revision: u64,
    pub(crate) log: &'a HistoryLog,
}

fn write_all(
    conn: &mut Connection,
    project: &Project,
    ver: &Versioning<'_>,
    extra: &[(&str, String)],
) -> Result<(), ProjectError> {
    let tx = conn.transaction().map_err(db_err)?;
    tx.execute_batch(SCHEMA_SQL).map_err(db_err)?;
    {
        let mut meta = tx.prepare("INSERT INTO meta (key, value) VALUES (?1, ?2)").map_err(db_err)?;
        meta.execute(params!["application", "rawgraph"]).map_err(db_err)?;
        meta.execute(params!["schema_version", SCHEMA_VERSION.to_string()]).map_err(db_err)?;
        meta.execute(params!["next_image_id", project.next_image_id.to_string()]).map_err(db_err)?;
        meta.execute(params!["project_uuid", project.uuid]).map_err(db_err)?;
        meta.execute(params!["revision", ver.revision.to_string()]).map_err(db_err)?;
        for (key, value) in extra {
            meta.execute(params![key, value]).map_err(db_err)?;
        }
    }
    {
        let mut ins = tx
            .prepare(
                "INSERT INTO images (position, id, uuid, path, content_hash, file_size, capture, rating, flag, color_label, missing, modified_unix_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            )
            .map_err(db_err)?;
        for (pos, e) in project.images.iter().enumerate() {
            let path = e
                .path
                .to_str()
                .ok_or_else(|| ProjectError::Io(format!("path {} is not valid UTF-8", e.path.display())))?;
            let capture = serde_json::to_string(&e.capture).map_err(|err| ProjectError::Corrupt(err.to_string()))?;
            ins.execute(params![
                to_i64(pos as u64, "position")?,
                to_i64(e.id.0, "image id")?,
                e.uuid,
                path,
                e.content_hash,
                to_i64(e.file_size, "file size")?,
                capture,
                i64::from(e.rating),
                flag_to_text(e.flag),
                e.color_label.map(label_to_text),
                i64::from(e.missing),
                e.modified_unix_ms,
            ])
            .map_err(db_err)?;
        }
    }
    tx.execute("INSERT INTO graph (id, json) VALUES (1, ?1)", params![project.graph.to_json()]).map_err(db_err)?;
    {
        let mut ins = tx
            .prepare("INSERT INTO folders (position, path, recursive) VALUES (?1, ?2, ?3)")
            .map_err(db_err)?;
        for (pos, f) in project.folders.iter().enumerate() {
            let path = f
                .path
                .to_str()
                .ok_or_else(|| ProjectError::Io(format!("path {} is not valid UTF-8", f.path.display())))?;
            ins.execute(params![to_i64(pos as u64, "position")?, path, i64::from(f.recursive)]).map_err(db_err)?;
        }
    }
    {
        let mut ins = tx
            .prepare(
                "INSERT INTO history (revision, time_unix_ms, author, device, summary, graph_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )
            .map_err(db_err)?;
        for (e, snap) in ver.log.entries.iter().zip(&ver.log.snapshots) {
            ins.execute(params![
                to_i64(e.revision, "revision")?,
                e.time_unix_ms,
                e.author,
                e.device,
                e.summary,
                snap.as_deref(),
            ])
            .map_err(db_err)?;
        }
    }
    tx.commit().map_err(db_err)
}

/// Writes the project's current state (its own revision and history). An
/// existing file of the same project and the current schema is updated in
/// place (only what differs); anything else is written as a whole new file.
pub(crate) fn save(project: &Project, path: &Path) -> Result<(), ProjectError> {
    if path.is_file() && crate::writemode::write_mode(path).mode == crate::writemode::WriteMode::InPlace {
        let own = (project.revision, project.history.clone());
        if let InPlace::Written { .. } = update_in_place(project, path, &mut |_| Ok(own.clone()))? {
            return Ok(());
        }
    }
    let ver = Versioning { revision: project.revision, log: &project.history };
    write_file(project, &ver, path, &[], true)
}

/// Like [`save`] with additional `meta` rows (used by recovery files). An
/// older file at `path` is not backed up: recovery files are disposable.
pub(crate) fn save_with_meta(project: &Project, path: &Path, extra: &[(&str, String)]) -> Result<(), ProjectError> {
    let ver = Versioning { revision: project.revision, log: &project.history };
    write_file(project, &ver, path, extra, false)
}

/// Writes a complete new file next to the target and swaps it in, so a
/// failure at any point leaves the previous project file untouched. With
/// `backup`, an existing project file of another schema version is first
/// copied next to it (`<name>.v<N>.bak`, never overwriting a copy).
pub(crate) fn write_file(
    project: &Project,
    ver: &Versioning<'_>,
    path: &Path,
    extra: &[(&str, String)],
    backup: bool,
) -> Result<(), ProjectError> {
    let mut tmp_name: OsString = path.as_os_str().to_owned();
    tmp_name.push(".saving");
    let tmp = PathBuf::from(tmp_name);
    let _ = std::fs::remove_file(&tmp);

    let result = (|| {
        let mut conn = Connection::open(&tmp).map_err(db_err)?;
        write_all(&mut conn, project, ver, extra)?;
        conn.close().map_err(|(_, e)| db_err(e))?;
        settle_journal(path)?;
        if backup {
            backup_other_version(path)?;
        }
        std::fs::rename(&tmp, path).map_err(|e| ProjectError::Io(format!("{}: {e}", path.display())))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Schema version recorded in a project file, `None` if it is not readable as one.
fn file_schema_version(path: &Path) -> Option<u32> {
    let conn = open_read(path).ok()?;
    let text: String = conn
        .query_row("SELECT value FROM meta WHERE key = 'schema_version'", [], |r| r.get(0))
        .optional()
        .ok()??;
    text.trim().parse().ok()
}

/// Copies an existing project file of a different schema version to
/// `<name>.v<N>.bak`, `<name>.v<N> (2).bak`, ... (an existing copy is never replaced).
fn backup_other_version(path: &Path) -> Result<(), ProjectError> {
    if !path.is_file() {
        return Ok(());
    }
    let Some(version) = file_schema_version(path) else { return Ok(()) };
    if version == SCHEMA_VERSION {
        return Ok(());
    }
    let Some(name) = path.file_name() else { return Ok(()) };
    for n in 1..=10_000u32 {
        let mut bak_name: OsString = name.to_owned();
        if n == 1 {
            bak_name.push(format!(".v{version}.bak"));
        } else {
            bak_name.push(format!(".v{version} ({n}).bak"));
        }
        let bak = path.with_file_name(bak_name);
        let mut target = match std::fs::OpenOptions::new().write(true).create_new(true).open(&bak) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(ProjectError::Io(format!("{}: {e}", bak.display()))),
        };
        let copied = std::fs::File::open(path).and_then(|mut src| std::io::copy(&mut src, &mut target));
        if let Err(e) = copied.and_then(|_| target.sync_all().map(|_| ())) {
            drop(target);
            let _ = std::fs::remove_file(&bak);
            return Err(ProjectError::Io(format!("{}: {e}", bak.display())));
        }
        return Ok(());
    }
    Err(ProjectError::Io(format!("too many backup copies of {} next to it", path.display())))
}

/// What a commit needs to know about the file it is going to replace.
pub(crate) struct DiskIdentity {
    pub(crate) version: u32,
    /// `None` for schema 1 files, which carry no project id.
    pub(crate) uuid: Option<String>,
    pub(crate) revision: u64,
}

pub(crate) fn read_identity(path: &Path) -> Result<DiskIdentity, ProjectError> {
    let conn = open_read(path)?;
    let version = read_version(&conn)?;
    // Schema 1 has no project id; a newer schema is not interpreted.
    if !(2..=SCHEMA_VERSION).contains(&version) {
        return Ok(DiskIdentity { version, uuid: None, revision: 0 });
    }
    let uuid = get_meta(&conn, "project_uuid")?.ok_or_else(|| ProjectError::Corrupt("missing project_uuid".into()))?;
    let revision = parse_revision(&conn)?;
    Ok(DiskIdentity { version, uuid: Some(uuid), revision })
}

fn get_meta(conn: &Connection, key: &str) -> Result<Option<String>, ProjectError> {
    conn.query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| r.get::<_, String>(0))
        .optional()
        .map_err(db_err)
}

fn parse_revision(conn: &Connection) -> Result<u64, ProjectError> {
    let text = get_meta(conn, "revision")?.ok_or_else(|| ProjectError::Corrupt("missing revision".into()))?;
    text.trim().parse().map_err(|_| ProjectError::Corrupt(format!("invalid revision \"{text}\"")))
}

fn read_version(conn: &Connection) -> Result<u32, ProjectError> {
    let text = get_meta(conn, "schema_version")?.ok_or_else(|| ProjectError::Corrupt("missing schema_version".into()))?;
    let version: u32 = text
        .trim()
        .parse()
        .map_err(|_| ProjectError::Corrupt(format!("invalid schema_version \"{text}\"")))?;
    if version == 0 {
        return Err(ProjectError::Corrupt("invalid schema_version 0".into()));
    }
    Ok(version)
}

struct RawRow {
    id: i64,
    uuid: String,
    path: String,
    hash: String,
    size: i64,
    capture: String,
    rating: i64,
    flag: String,
    label: Option<String>,
    missing: i64,
    mtime: Option<i64>,
}

/// Loads a project file. A file with an older schema version is migrated in
/// memory (the file itself is not touched); a newer one gives
/// [`ProjectError::TooNew`].
pub(crate) fn load(path: &Path) -> Result<Project, ProjectError> {
    let meta = std::fs::metadata(path).map_err(|e| ProjectError::Io(format!("{}: {e}", path.display())))?;
    if !meta.is_file() {
        return Err(ProjectError::Io(format!("{} is not a file", path.display())));
    }
    let conn = open_read(path)?;
    let version = read_version(&conn)?;
    if version > SCHEMA_VERSION {
        return Err(ProjectError::TooNew { file_version: version, supported: SCHEMA_VERSION });
    }
    if version < SCHEMA_VERSION {
        let mem = copy_to_memory(&conn)?;
        drop(conn);
        migrate(&mem, version)?;
        let mut project = read_current(&mem)?;
        project.migrated_from = Some(version);
        return Ok(project);
    }
    read_current(&conn)
}

/// Copies every table (definition and rows) of `src` into a new in-memory database.
fn copy_to_memory(src: &Connection) -> Result<Connection, ProjectError> {
    let mem = Connection::open_in_memory().map_err(db_err)?;
    let tables: Vec<(String, String)> = {
        let mut stmt = src
            .prepare("SELECT name, sql FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' AND sql IS NOT NULL")
            .map_err(db_err)?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(db_err)?;
        rows.collect::<Result<_, _>>().map_err(db_err)?
    };
    for (name, sql) in tables {
        mem.execute_batch(&sql).map_err(db_err)?;
        let quoted = format!("\"{}\"", name.replace('"', "\"\""));
        let mut sel = src.prepare(&format!("SELECT * FROM {quoted}")).map_err(db_err)?;
        let cols = sel.column_count();
        let marks: Vec<String> = (1..=cols).map(|i| format!("?{i}")).collect();
        let mut ins = mem
            .prepare(&format!("INSERT INTO {quoted} VALUES ({})", marks.join(", ")))
            .map_err(db_err)?;
        let mut rows = sel.query([]).map_err(db_err)?;
        while let Some(row) = rows.next().map_err(db_err)? {
            let values: Vec<Value> = (0..cols).map(|i| row.get(i)).collect::<Result<_, _>>().map_err(db_err)?;
            ins.execute(params_from_iter(values.iter())).map_err(db_err)?;
        }
    }
    Ok(mem)
}

/// Runs the steps from `from` up to [`SCHEMA_VERSION`] on `conn`.
fn migrate(conn: &Connection, from: u32) -> Result<(), ProjectError> {
    let mut version = from;
    while version < SCHEMA_VERSION {
        let (_, step) = MIGRATIONS
            .iter()
            .find(|(f, _)| *f == version)
            .ok_or_else(|| ProjectError::Corrupt(format!("no migration from schema version {version}")))?;
        step(conn)?;
        version += 1;
        conn.execute("UPDATE meta SET value = ?1 WHERE key = 'schema_version'", params![version.to_string()])
            .map_err(db_err)?;
    }
    Ok(())
}

/// Schema 1 -> 2: image uuids, project uuid, revision, `folders`, `history`.
fn migrate_1_to_2(conn: &Connection) -> Result<(), ProjectError> {
    conn.execute_batch(
        "ALTER TABLE images ADD COLUMN uuid TEXT;
         CREATE TABLE folders (
             position  INTEGER PRIMARY KEY NOT NULL,
             path      TEXT NOT NULL,
             recursive INTEGER NOT NULL
         );
         CREATE TABLE history (
             revision      INTEGER PRIMARY KEY NOT NULL,
             time_unix_ms  INTEGER NOT NULL,
             author        TEXT NOT NULL,
             device        TEXT NOT NULL,
             summary       TEXT NOT NULL,
             graph_json    TEXT
         );",
    )
    .map_err(db_err)?;
    let positions: Vec<i64> = {
        let mut stmt = conn.prepare("SELECT position FROM images").map_err(db_err)?;
        let rows = stmt.query_map([], |r| r.get::<_, i64>(0)).map_err(db_err)?;
        rows.collect::<Result<_, _>>().map_err(db_err)?
    };
    {
        let mut upd = conn.prepare("UPDATE images SET uuid = ?1 WHERE position = ?2").map_err(db_err)?;
        for pos in positions {
            upd.execute(params![ids::new_uuid(), pos]).map_err(db_err)?;
        }
    }
    conn.execute_batch("CREATE UNIQUE INDEX images_uuid ON images (uuid);").map_err(db_err)?;
    conn.execute("INSERT INTO meta (key, value) VALUES ('project_uuid', ?1)", params![ids::new_uuid()])
        .map_err(db_err)?;
    conn.execute("INSERT INTO meta (key, value) VALUES ('revision', '0')", []).map_err(db_err)?;
    Ok(())
}

/// Reads a database that is in the current schema.
fn read_current(conn: &Connection) -> Result<Project, ProjectError> {
    let next_text = get_meta(conn, "next_image_id")?.ok_or_else(|| ProjectError::Corrupt("missing next_image_id".into()))?;
    let next_image_id: u64 = next_text
        .trim()
        .parse()
        .map_err(|_| ProjectError::Corrupt(format!("invalid next_image_id \"{next_text}\"")))?;
    if next_image_id == 0 {
        return Err(ProjectError::Corrupt("next_image_id must be at least 1".into()));
    }
    let uuid = get_meta(conn, "project_uuid")?.ok_or_else(|| ProjectError::Corrupt("missing project_uuid".into()))?;
    if !ids::is_uuid(&uuid) {
        return Err(ProjectError::Corrupt(format!("invalid project_uuid \"{uuid}\"")));
    }
    let revision = parse_revision(conn)?;

    let graph_json: String = conn
        .query_row("SELECT json FROM graph WHERE id = 1", [], |r| r.get(0))
        .optional()
        .map_err(db_err)?
        .ok_or_else(|| ProjectError::Corrupt("missing graph".into()))?;
    let graph = Graph::from_json(&graph_json).map_err(|e| ProjectError::Corrupt(e.to_string()))?;

    let mut stmt = conn
        .prepare(
            "SELECT id, uuid, path, content_hash, file_size, capture, rating, flag, color_label, missing, modified_unix_ms
             FROM images ORDER BY position",
        )
        .map_err(db_err)?;
    let rows = stmt
        .query_map([], |r| {
            Ok(RawRow {
                id: r.get(0)?,
                uuid: r.get(1)?,
                path: r.get(2)?,
                hash: r.get(3)?,
                size: r.get(4)?,
                capture: r.get(5)?,
                rating: r.get(6)?,
                flag: r.get(7)?,
                label: r.get(8)?,
                missing: r.get(9)?,
                mtime: r.get(10)?,
            })
        })
        .map_err(db_err)?;

    let mut images: Vec<ImageEntry> = Vec::new();
    for row in rows {
        let row = row.map_err(db_err)?;
        let id = to_u64(row.id, "image id")?;
        if id == 0 || id >= next_image_id {
            return Err(ProjectError::Corrupt(format!("image id {id} is outside the allocated range")));
        }
        if images.last().is_some_and(|prev| prev.id.0 >= id) {
            return Err(ProjectError::Corrupt("image ids are not strictly ascending".into()));
        }
        if !ids::is_uuid(&row.uuid) {
            return Err(ProjectError::Corrupt(format!("invalid uuid \"{}\" of image {id}", row.uuid)));
        }
        let rating = u8::try_from(row.rating)
            .ok()
            .filter(|r| *r <= 5)
            .ok_or_else(|| ProjectError::Corrupt(format!("rating {} is out of range", row.rating)))?;
        let capture: CaptureInfo = serde_json::from_str(&row.capture)
            .map_err(|e| ProjectError::Corrupt(format!("capture data of image {id}: {e}")))?;
        images.push(ImageEntry {
            id: ImageId(id),
            uuid: row.uuid,
            path: row.path.into(),
            content_hash: row.hash,
            file_size: to_u64(row.size, "file size")?,
            modified_unix_ms: row.mtime,
            capture,
            rating,
            flag: flag_from_text(&row.flag)?,
            color_label: row.label.as_deref().map(label_from_text).transpose()?,
            missing: row.missing != 0,
        });
    }
    drop(stmt);

    let folders: Vec<SourceFolder> = {
        let mut stmt = conn
            .prepare("SELECT path, recursive FROM folders ORDER BY position")
            .map_err(db_err)?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .map_err(db_err)?;
        let mut out = Vec::new();
        for row in rows {
            let (path, recursive) = row.map_err(db_err)?;
            out.push(SourceFolder { path: path.into(), recursive: recursive != 0 });
        }
        out
    };

    let history = {
        let mut stmt = conn
            .prepare("SELECT revision, time_unix_ms, author, device, summary, graph_json FROM history ORDER BY revision")
            .map_err(db_err)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, Option<String>>(5)?,
                ))
            })
            .map_err(db_err)?;
        let mut entries: Vec<HistoryEntry> = Vec::new();
        let mut snapshots: Vec<Option<Arc<str>>> = Vec::new();
        for row in rows {
            let (rev, time, author, device, summary, graph) = row.map_err(db_err)?;
            entries.push(HistoryEntry {
                revision: to_u64(rev, "history revision")?,
                time_unix_ms: time,
                author,
                device,
                summary,
                has_graph: graph.is_some(),
            });
            snapshots.push(graph.map(Arc::from));
        }
        HistoryLog::from_parts(entries, snapshots)
    };
    if history.entries.last().is_some_and(|e| e.revision > revision) {
        return Err(ProjectError::Corrupt("history is newer than the project revision".into()));
    }

    Ok(Project {
        graph,
        uuid,
        images,
        next_image_id,
        revision,
        history,
        folders,
        migrated_from: None,
        scan_threads: crate::DEFAULT_SCAN_THREADS,
    })
}

/// All `meta` rows of a project file plus its image count, read-only.
pub(crate) fn read_meta(path: &Path) -> Result<(std::collections::HashMap<String, String>, usize), ProjectError> {
    let conn = open_read(path)?;
    let mut map = std::collections::HashMap::new();
    {
        let mut stmt = conn.prepare("SELECT key, value FROM meta").map_err(db_err)?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).map_err(db_err)?;
        for row in rows {
            let (k, v) = row.map_err(db_err)?;
            map.insert(k, v);
        }
    }
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM images", [], |r| r.get(0)).map_err(db_err)?;
    Ok((map, usize::try_from(count).unwrap_or(0)))
}

#[cfg(test)]
pub(crate) fn migration_steps() -> Vec<u32> {
    MIGRATIONS.iter().map(|(f, _)| *f).collect()
}

// ------------------------------------------------------------------ in-place update

/// How long one lock request waits for another process that holds the file locked (a save asks twice: write lock, then commit; so at most about 4 s).
const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

// Test hook (per thread): the next in-place update fails after all rows were written but
// before the transaction is committed (crash-safety tests).
#[cfg(test)]
thread_local! {
    pub(crate) static FAIL_BEFORE_COMMIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Decides revision and history of an in-place update from the identity of the file (or refuses).
pub(crate) type Prepare<'a> = dyn FnMut(&DiskIdentity) -> Result<(u64, HistoryLog), ProjectError> + 'a;

/// Outcome of [`update_in_place`].
pub(crate) enum InPlace {
    /// The file now holds the project, with this revision and history.
    Written { revision: u64, log: HistoryLog },
    /// The target is not a current-schema file of this project (or cannot be
    /// updated row by row): the caller writes the whole file instead.
    NotApplicable,
}

/// Internal: why a row-by-row update stopped.
enum DiffError {
    /// A constraint did not allow the row-wise change; a full rewrite will do it.
    Fallback,
    Fatal(ProjectError),
}

impl From<rusqlite::Error> for DiffError {
    fn from(e: rusqlite::Error) -> Self {
        if matches!(&e, rusqlite::Error::SqliteFailure(f, _) if f.code == ErrorCode::ConstraintViolation) {
            DiffError::Fallback
        } else {
            DiffError::Fatal(db_err(e))
        }
    }
}

impl From<ProjectError> for DiffError {
    fn from(e: ProjectError) -> Self {
        DiffError::Fatal(e)
    }
}

fn identity_in(conn: &Connection) -> Option<DiskIdentity> {
    let version = read_version(conn).ok()?;
    if version != SCHEMA_VERSION {
        return None;
    }
    let uuid = get_meta(conn, "project_uuid").ok()??;
    let revision = parse_revision(conn).ok()?;
    Some(DiskIdentity { version, uuid: Some(uuid), revision })
}

/// Updates an existing project file of the current schema in place, in ONE
/// transaction of SQLite's rollback journal: only the image rows, the graph,
/// the folders and the `meta` values that differ are written, history entries
/// are appended (pruned ones deleted), old ones are not rewritten.
///
/// The transaction takes the write lock first (`BEGIN IMMEDIATE`), reads the
/// file's identity under that lock and hands it to `prepare`, which decides
/// the new revision and history or refuses (the revision/conflict check). So
/// the check and the write are one atomic step with respect to other writers.
/// A failure of any kind rolls the transaction back and leaves the old content.
///
/// The rollback journal (not WAL) is used on purpose: WAL needs shared memory
/// and does not work on network shares (SMB/NFS) or synced folders; the
/// rollback journal is a plain file next to the project file.
pub(crate) fn update_in_place(
    project: &Project,
    path: &Path,
    prepare: &mut Prepare<'_>,
) -> Result<InPlace, ProjectError> {
    let Ok(mut conn) = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE) else {
        return Ok(InPlace::NotApplicable);
    };
    let _ = conn.busy_timeout(BUSY_TIMEOUT);
    let tx = match conn.transaction_with_behavior(TransactionBehavior::Immediate) {
        Ok(tx) => tx,
        Err(e) => {
            return match &e {
                rusqlite::Error::SqliteFailure(f, _) if matches!(f.code, ErrorCode::NotADatabase | ErrorCode::DatabaseCorrupt) => Ok(InPlace::NotApplicable),
                _ => Err(db_err(e)),
            };
        }
    };
    let Some(identity) = identity_in(&tx) else { return Ok(InPlace::NotApplicable) };
    if identity.uuid.as_deref() != Some(project.uuid.as_str()) {
        return Ok(InPlace::NotApplicable);
    }
    let (revision, log) = prepare(&identity)?;
    match apply_diff(&tx, project, &Versioning { revision, log: &log }) {
        Ok(()) => {}
        Err(DiffError::Fallback) => return Ok(InPlace::NotApplicable),
        Err(DiffError::Fatal(e)) => return Err(e),
    }
    #[cfg(test)]
    if FAIL_BEFORE_COMMIT.with(|f| f.replace(false)) {
        return Err(ProjectError::Database("injected failure before commit".into()));
    }
    tx.commit().map_err(db_err)?;
    Ok(InPlace::Written { revision, log })
}

/// One `images` row without its position.
#[derive(PartialEq)]
struct ImageRow {
    uuid: String,
    path: String,
    hash: String,
    size: i64,
    mtime: Option<i64>,
    capture: String,
    rating: i64,
    flag: String,
    label: Option<String>,
    missing: i64,
}

fn image_row(e: &ImageEntry) -> Result<ImageRow, ProjectError> {
    let path = e
        .path
        .to_str()
        .ok_or_else(|| ProjectError::Io(format!("path {} is not valid UTF-8", e.path.display())))?;
    Ok(ImageRow {
        uuid: e.uuid.clone(),
        path: path.to_owned(),
        hash: e.content_hash.clone(),
        size: to_i64(e.file_size, "file size")?,
        mtime: e.modified_unix_ms,
        capture: serde_json::to_string(&e.capture).map_err(|err| ProjectError::Corrupt(err.to_string()))?,
        rating: i64::from(e.rating),
        flag: flag_to_text(e.flag).to_owned(),
        label: e.color_label.map(label_to_text).map(str::to_owned),
        missing: i64::from(e.missing),
    })
}

fn apply_diff(tx: &Connection, project: &Project, ver: &Versioning<'_>) -> Result<(), DiffError> {
    {
        let mut up = tx.prepare(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value WHERE value IS NOT excluded.value",
        )?;
        up.execute(params!["application", "rawgraph"])?;
        up.execute(params!["schema_version", SCHEMA_VERSION.to_string()])?;
        up.execute(params!["next_image_id", project.next_image_id.to_string()])?;
        up.execute(params!["project_uuid", project.uuid])?;
        up.execute(params!["revision", ver.revision.to_string()])?;
        // Anything else (e.g. `recovery_*` keys of a recovery copy) does not belong in a project file: same result as a full write.
        tx.execute("DELETE FROM meta WHERE key NOT IN ('application', 'schema_version', 'next_image_id', 'project_uuid', 'revision')", [])?;
    }
    diff_images(tx, project)?;
    diff_graph(tx, project)?;
    diff_folders(tx, project)?;
    diff_history(tx, ver)?;
    Ok(())
}

fn diff_images(tx: &Connection, project: &Project) -> Result<(), DiffError> {
    let mut old: HashMap<i64, (i64, ImageRow)> = HashMap::new();
    {
        let mut stmt = tx.prepare(
            "SELECT id, position, uuid, path, content_hash, file_size, modified_unix_ms, capture, rating, flag, color_label, missing FROM images",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                ImageRow {
                    uuid: r.get(2)?,
                    path: r.get(3)?,
                    hash: r.get(4)?,
                    size: r.get(5)?,
                    mtime: r.get(6)?,
                    capture: r.get(7)?,
                    rating: r.get(8)?,
                    flag: r.get(9)?,
                    label: r.get(10)?,
                    missing: r.get(11)?,
                },
            ))
        })?;
        for row in rows {
            let (id, pos, fields) = row?;
            old.insert(id, (pos, fields));
        }
    }
    let mut new: Vec<(i64, i64, ImageRow)> = Vec::with_capacity(project.images.len());
    for (pos, e) in project.images.iter().enumerate() {
        new.push((to_i64(e.id.0, "image id")?, to_i64(pos as u64, "position")?, image_row(e)?));
    }
    // 1. Rows that are gone.
    {
        let keep: HashSet<i64> = new.iter().map(|(id, _, _)| *id).collect();
        let mut del = tx.prepare("DELETE FROM images WHERE id = ?1")?;
        for id in old.keys().filter(|id| !keep.contains(id)) {
            del.execute(params![id])?;
        }
    }
    // 2. Rows that change position leave their old slot first, so no two rows
    //    ever share a position (the key) halfway through.
    {
        let mut park = tx.prepare("UPDATE images SET position = ?1 WHERE id = ?2")?;
        for (id, pos, _) in &new {
            if old.get(id).is_some_and(|(old_pos, _)| old_pos != pos) {
                park.execute(params![-pos - 1, id])?;
            }
        }
    }
    // 3. New rows, changed rows, rows that only moved.
    let mut ins = tx.prepare(
        "INSERT INTO images (position, id, uuid, path, content_hash, file_size, capture, rating, flag, color_label, missing, modified_unix_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
    )?;
    let mut upd = tx.prepare(
        "UPDATE images SET position = ?1, uuid = ?3, path = ?4, content_hash = ?5, file_size = ?6, capture = ?7, rating = ?8, flag = ?9,
                color_label = ?10, missing = ?11, modified_unix_ms = ?12 WHERE id = ?2",
    )?;
    let mut mv = tx.prepare("UPDATE images SET position = ?1 WHERE id = ?2")?;
    for (id, pos, r) in &new {
        let values = params![pos, id, r.uuid, r.path, r.hash, r.size, r.capture, r.rating, r.flag, r.label, r.missing, r.mtime];
        match old.get(id) {
            None => {
                ins.execute(values)?;
            }
            Some((_, old_fields)) if old_fields != r => {
                upd.execute(values)?;
            }
            Some((old_pos, _)) if old_pos != pos => {
                mv.execute(params![pos, id])?;
            }
            Some(_) => {}
        }
    }
    Ok(())
}

fn diff_graph(tx: &Connection, project: &Project) -> Result<(), DiffError> {
    let json = project.graph.to_json();
    let old: Option<String> = tx.query_row("SELECT json FROM graph WHERE id = 1", [], |r| r.get(0)).optional()?;
    match old {
        Some(old) if old == json => {}
        Some(_) => {
            tx.execute("UPDATE graph SET json = ?1 WHERE id = 1", params![json])?;
        }
        None => {
            tx.execute("INSERT INTO graph (id, json) VALUES (1, ?1)", params![json])?;
        }
    }
    Ok(())
}

fn diff_folders(tx: &Connection, project: &Project) -> Result<(), DiffError> {
    let mut new: Vec<(String, i64)> = Vec::new();
    for f in &project.folders {
        let path = f
            .path
            .to_str()
            .ok_or_else(|| ProjectError::Io(format!("path {} is not valid UTF-8", f.path.display())))?;
        new.push((path.to_owned(), i64::from(f.recursive)));
    }
    let old: Vec<(String, i64)> = {
        let mut stmt = tx.prepare("SELECT path, recursive FROM folders ORDER BY position")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        rows.collect::<Result<_, _>>()?
    };
    if old == new {
        return Ok(());
    }
    tx.execute("DELETE FROM folders", [])?;
    let mut ins = tx.prepare("INSERT INTO folders (position, path, recursive) VALUES (?1, ?2, ?3)")?;
    for (pos, (path, recursive)) in new.iter().enumerate() {
        ins.execute(params![to_i64(pos as u64, "position")?, path, recursive])?;
    }
    Ok(())
}

fn diff_history(tx: &Connection, ver: &Versioning<'_>) -> Result<(), DiffError> {
    /// What a stored history row looks like without reading its snapshot.
    #[derive(PartialEq)]
    struct Meta {
        time: i64,
        author: String,
        device: String,
        summary: String,
    }
    let mut old: HashMap<i64, (Meta, bool)> = HashMap::new();
    {
        let mut stmt = tx.prepare(
            "SELECT revision, time_unix_ms, author, device, summary, graph_json IS NOT NULL FROM history",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                Meta { time: r.get(1)?, author: r.get(2)?, device: r.get(3)?, summary: r.get(4)? },
                r.get::<_, bool>(5)?,
            ))
        })?;
        for row in rows {
            let (revision, meta, has) = row?;
            old.insert(revision, (meta, has));
        }
    }
    let mut revisions = HashSet::new();
    for e in &ver.log.entries {
        revisions.insert(to_i64(e.revision, "revision")?);
    }
    {
        let mut del = tx.prepare("DELETE FROM history WHERE revision = ?1")?;
        for revision in old.keys().filter(|r| !revisions.contains(r)) {
            del.execute(params![revision])?;
        }
    }
    let mut put = tx.prepare(
        "INSERT OR REPLACE INTO history (revision, time_unix_ms, author, device, summary, graph_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    let mut drop_snapshot = tx.prepare("UPDATE history SET graph_json = NULL WHERE revision = ?1")?;
    for (e, snap) in ver.log.entries.iter().zip(&ver.log.snapshots) {
        let revision = to_i64(e.revision, "revision")?;
        let meta = Meta { time: e.time_unix_ms, author: e.author.clone(), device: e.device.clone(), summary: e.summary.clone() };
        match old.get(&revision) {
            // A snapshot of a revision never changes, so metadata and presence are enough (the snapshots, up to 32 MB, are not read).
            Some((old_meta, has)) if *old_meta == meta && *has == snap.is_some() => {}
            Some((old_meta, true)) if *old_meta == meta && snap.is_none() => {
                drop_snapshot.execute(params![revision])?;
            }
            _ => {
                put.execute(params![revision, e.time_unix_ms, e.author, e.device, e.summary, snap.as_deref()])?;
            }
        }
    }
    Ok(())
}

fn journal_of(path: &Path) -> PathBuf {
    let mut journal: OsString = path.as_os_str().to_owned();
    journal.push("-journal");
    PathBuf::from(journal)
}

/// Opens a project file for reading, always read-only, and starts ONE read
/// transaction: everything read through the connection comes from a single
/// consistent state of the file, even if somebody commits meanwhile. The caller
/// just drops the connection.
///
/// Reading never writes. A rollback journal next to the file (`<name>-journal`)
/// that SQLite would have to replay (an interrupted save) is not replayed here:
/// a reader may be on another computer, or the "interrupted" save may be a live
/// one on a share with weak locking. The error says what to do;
/// [`recover_unfinished_save`] is the explicit way.
fn open_read(path: &Path) -> Result<Connection, ProjectError> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(db_err)?;
    let _ = conn.busy_timeout(BUSY_TIMEOUT);
    let started = conn
        .execute_batch("BEGIN")
        .and_then(|()| conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get::<_, i64>(0)).map(|_| ()));
    match started {
        Ok(()) => Ok(conn),
        Err(e) if journal_of(path).exists() && !is_busy(&e) => Err(ProjectError::Io(unfinished_save_text(path))),
        Err(e) => Err(db_err(e)),
    }
}

fn unfinished_save_text(path: &Path) -> String {
    let journal = journal_of(path);
    format!(
        "the project file has an unfinished save next to it ({}): open it from the computer that saved it, or, if no program is saving it any more, use \"Recover unfinished save\" (this rolls the file back to its last complete state)",
        journal.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    )
}

fn is_busy(e: &rusqlite::Error) -> bool {
    matches!(e, rusqlite::Error::SqliteFailure(f, _) if matches!(f.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked))
}

/// Rolls an interrupted save back: opens the file read-write once so SQLite
/// replays the hot `<name>-journal` onto it, which restores the last complete
/// state. Only to be called on explicit request of the user, on the computer
/// that owns the file, when no program is saving it. Returns whether a journal
/// was there.
pub(crate) fn recover_unfinished_save(path: &Path) -> Result<bool, ProjectError> {
    let had = journal_of(path).exists();
    if !had {
        return Ok(false);
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE).map_err(db_err)?;
    let _ = conn.busy_timeout(BUSY_TIMEOUT);
    conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get::<_, i64>(0)).map_err(db_err)?;
    drop(conn);
    if journal_of(path).exists() {
        return Err(ProjectError::Io(format!("{} is still there after the recovery: another program may be saving the file", journal_of(path).display())));
    }
    Ok(true)
}

/// Before a full write replaces `path`: a journal beside the old file must not
/// survive next to the new one (SQLite would replay it onto the new file). It
/// is rolled back on the old file (this is the write path, so a read-write open
/// is fine); if it cannot be settled the save is refused.
fn settle_journal(path: &Path) -> Result<(), ProjectError> {
    if !path.is_file() || !journal_of(path).exists() {
        return Ok(());
    }
    recover_unfinished_save(path).map(|_| ())
}
