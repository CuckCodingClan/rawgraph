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

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rawgraph_core::{CaptureInfo, Graph, ImageId};
use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, Connection, ErrorCode, OpenFlags, OptionalExtension};

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
    }
    ProjectError::Database(e.to_string())
}

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

/// Writes the project's current state (its own revision and history).
pub(crate) fn save(project: &Project, path: &Path) -> Result<(), ProjectError> {
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
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
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
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(db_err)?;
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
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(db_err)?;
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
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(db_err)?;
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
