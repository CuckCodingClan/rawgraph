# RawGraph project file format (schema 2)

Implemented by the crate `rawgraph-project`. A project is one SQLite database
(extension `.rawgraph`, rusqlite with bundled SQLite). Source images are never
part of it.

## Tables

| Table | Columns |
|---|---|
| `meta` | `key` TEXT PK, `value` TEXT. Keys: `application` (`rawgraph`), `schema_version`, `next_image_id`, `project_uuid`, `revision`. Recovery copies add `recovery_*` keys. |
| `images` | `position` INTEGER PK (project order), `id` INTEGER UNIQUE (numeric image id used by graphs), `uuid` TEXT UNIQUE, `path` TEXT (absolute), `content_hash` TEXT, `file_size` INTEGER, `modified_unix_ms` INTEGER NULL, `capture` TEXT (JSON of the camera metadata), `rating` 0..5, `flag` (`none`/`pick`/`reject`), `color_label` NULL, `missing` 0/1 |
| `graph` | `id` INTEGER PK (always 1), `json` TEXT (the node graph, with its own `format_version`) |
| `folders` | `position` INTEGER PK, `path` TEXT, `recursive` 0/1: folders that were scanned into the project |
| `history` | `revision` INTEGER PK, `time_unix_ms`, `author`, `device`, `summary` (all NOT NULL), `graph_json` TEXT NULL (the graph of that revision, NULL once dropped) |

A file is always written as a whole, in one transaction, into `<name>.saving`
and then renamed over the target. A failed write leaves the old file in place.

## What is stored, what is not

- Stored: image entries (path, size, partial content hash, capture metadata,
  rating, flag, colour label, missing state), the graph with all per-image
  parameters, the scanned folders, the history.
- Not stored: image pixels, sidecars, thumbnails (a separate cache outside the
  project), UI state, the scan thread count. Source images are referenced by
  absolute path plus a partial BLAKE3 hash (file size, first and last 64 KiB),
  never embedded. The crate never writes, renames or deletes image files or
  sidecars; it only writes project files, `.bak` copies next to them, recovery
  files and the thumbnail cache.

## Versioning and migration

- `meta.schema_version` is an integer, `SCHEMA_VERSION` is the highest one this
  build writes (2).
- Loading a file with an older version copies it into an in-memory database,
  runs the ordered steps `vN -> vN+1` there, and reads the result. The file on
  disk is not modified by loading.
- Loading a newer version fails with `ProjectError::TooNew { file_version, supported }`.
  A plain `commit` onto such a file fails the same way.
- The first write over an older file (`save` or `commit`) first copies it to
  `<name>.v<N>.bak`; if that exists, `<name>.v<N> (2).bak`, `(3)`, and so on. An
  existing backup is never overwritten. A failed copy aborts the write.
- Adding schema 3: bump `SCHEMA_VERSION`, update the table SQL and reader/writer
  in `store.rs`, write `migrate_2_to_3(&Connection)` and append `(2, migrate_2_to_3)`
  to `MIGRATIONS`. A test fails if a step is missing.
- Migration 1 -> 2 adds `images.uuid` (generated per image), `project_uuid`,
  `revision = 0`, and empty `folders` and `history`. A fixture of a real schema 1
  file is in `crates/rawgraph-project/tests/fixtures/`. Each load of a schema 1
  file generates fresh uuids, because the file is not changed; they become
  permanent with the first write.

## Ids

- `project_uuid`: 32 lowercase hex characters, created with the project, kept by
  `save`, "save as" (a copy written elsewhere is the same project) and recovery
  copies.
- `images.uuid`: same format, created when the image is first added, never
  changed (relinking, rescans and saves keep it), never reused after removal.
- `images.id` (numeric) stays the key that graphs use. It is unique inside one
  project file only; the uuid is the id that is meaningful across files and
  people.
- uuids are 128 bits of BLAKE3 over SQLite `randomblob`, the operating system's
  randomness (via the standard library) and time, process id and a counter.

## Revisions, history, limits

- `revision` starts at 0 and is the number of the last commit. `commit(path, summary)`
  writes images, graph, folders and history in one transaction, increments the
  revision and appends a history entry: revision, time, author (OS user name from
  `USERNAME`/`USER`/`LOGNAME`), device (`COMPUTERNAME`/`HOSTNAME`, else `/etc/hostname`;
  `unknown` if none), summary, and a snapshot of the graph JSON.
- `save(path)` writes the current state including the existing history but creates
  no revision and does no conflict check.
- `history()` lists entries oldest first; `graph_at(revision)` returns the stored graph.
- Limits: at most 100 entries and 32 MiB of graph snapshots. Over the byte limit,
  the oldest snapshots are dropped first (the entry stays, `has_graph = false`);
  over the count limit the oldest entries are dropped.
- Only the graph is snapshotted. Ratings, flags, labels and the image list have no
  history; they are only in the current state.

## Conflict rule (optimistic)

A `Project` remembers the revision it was loaded with or last committed. `commit` on
an existing file first reads that file's `project_uuid` and `revision`:

- same project and disk revision higher than the remembered one: `ProjectError::Conflict
  { disk_revision, loaded_revision }`, nothing is written;
- other `project_uuid`: `ProjectError::Mismatch` ("belongs to another project");
- no file at the target: created, no check;
- schema 1 file: accepted only for a project that was loaded from a schema 1 file.

`commit_overwrite` skips these checks; its revision is `max(own, disk) + 1`, so a number is
not reused. The check and the rename are separate steps, so two commits at the same
instant can both pass. After a `Conflict` the caller reloads, or uses `commit_overwrite`.

## Collaboration foundations

Provided in this wave: stable project and image uuids, single-transaction commits,
the revision check, and a change history with author, device and graph snapshots.

Not provided: no merge of concurrent edits (a conflict means one side is lost or redone
by hand), no locking, no permissions or users beyond the OS name in the history, no network
or shared store, no asset-availability handling beyond the per-image `missing` flag.

### Deployment options for later

(a) Project file on a shared or synced folder (SMB, Nextcloud, Dropbox), same format.
- Works with the current revision check, no server. Whole-file replacement means the
  last writer to pass the check wins; sync tools that copy files while SQLite is
  being written, or that create "conflicted copy" files, bypass the check.
- A file of 2000+ images is rewritten per commit; two people editing different images
  still conflict. Data model consequences: none needed now; per-row timestamps would
  allow a three-way merge of ratings/flags later.

(b) Server database (e.g. PostgreSQL) with per-row revisions.
- Needs a `revision` (or `updated_at`) column per image row and per graph node, a
  change log table keyed by `project_uuid`, and writes as compare-and-set per row. This
  gives row-level conflicts instead of whole-project ones, locks per row or per node if
  wanted, and permissions as a server concern.
- Consequences: image `uuid` becomes the primary key (the numeric `id` is only
  unique per project, so the server maps it); the graph would have to be stored per
  node or as an operation log instead of one JSON blob to merge anything; paths
  need a per-device mapping (`path` is absolute on one machine); thumbnails and
  source images need their own availability tracking. A client still needs a local
  SQLite cache in this format for offline work.
