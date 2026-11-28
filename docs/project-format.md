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

## How a file is written

Two ways, both all-or-nothing. Which one is used is decided per file (see "Which way is used where").

- **In place** (`commit`, `commit_overwrite`, `save` onto an existing file that
  belongs to the same project and has the current schema version): one SQLite
  transaction updates the file where it lies. Only rows that differ are written:
  changed or new `images` rows (matched by `id`; rows that only moved get a new
  `position`), the `graph` row if its JSON changed, `folders` if the list changed,
  the `meta` values that changed (other `meta` keys are removed, so the result equals
  a full write), and the new `history` entry. Old history entries are not rewritten
  (and their snapshots are not read: a snapshot never changes, so metadata and
  "has a snapshot" decide); pruned ones are deleted; a snapshot dropped for size is set
  to NULL. What differs is found by reading the image rows, the graph and the history
  metadata inside the transaction (about 1-2 MB for 4,000 images plus the graph, not the
  snapshots) and comparing them with the project, not by tracking edits, so the result
  is the same as a full write whatever happened before. A failure (error, crash, power
  loss) rolls the transaction back and the file keeps its old content. If a row-wise
  change is impossible (a constraint fails) the full write below is used instead.
- **Full write** (new file, "save as" to a new path, another project's file,
  schema migration, recovery copies, and every file for which in place is not allowed):
  the whole database is written into `<name>.saving` in one transaction and renamed over
  the target. The target is complete at every instant, and no journal file is involved.
  A failed write leaves the old file in place. A file of an older schema is first copied
  to `<name>.v<N>.bak`. A `<name>-journal` next to the old file is settled first
  (rolled back on the old file; if that is not possible the save is refused), so a stale
  journal never stays next to the new file.

### Which way is used where

In place is used **only on a local fixed drive that is not inside a sync folder**. The
whole file is rewritten (full write) when the file is on:

- a **network drive**: UNC paths (`\\server\share`), mapped drives (Windows
  `GetDriveTypeW` is not "fixed"), removable drives; on Linux a mount of type
  nfs/nfs4/cifs/smb3/smbfs/9p/afs/ceph/glusterfs/davfs/ncpfs/lustre/sshfs/rclone
  (`/proc/self/mounts`); on macOS everything under `/Volumes`;
- a **sync folder**: a folder or any parent that holds a marker of a sync client:
  `.nextcloudsync.log`, `.owncloudsync.log`, `.sync_*.db` (Nextcloud/ownCloud),
  `.dropbox`, `.dropbox.cache` (Dropbox), or a path under the folders named by the
  environment variables `OneDrive`, `OneDriveConsumer`, `OneDriveCommercial`;
- any file when the setting **"Always rewrite the whole project file"** is on
  (Settings menu, default off, also `RAWGRAPH_ALWAYS_REWRITE=1`).

The choice is made once per 30 s per folder. `rawgraph_project::write_mode(path)` returns
it with the reason; the application shows it in the status line after a save, e.g.
"Saved ... (revision 7; whole file rewritten, inside a Nextcloud sync folder)".

Why: in place uses SQLite's rollback journal, a second file `<name>-journal` that exists
during a save. That is safe on a local disk but not where files are copied or locked by
somebody else:

- A sync client (Nextcloud, Dropbox, OneDrive) can upload the main file and the journal
  at different times. Another computer can then receive a changed main file with a
  journal, or a journal alone, and replaying a stray journal onto a healthy file
  corrupts it.
- On network shares with weak locking (SMB/NFS, some NAS), two writers can both believe
  they hold the write lock; and a reader that is allowed to write could roll back a
  transaction that another user is still running.
- WAL would be worse still (it needs shared memory between the processes of one machine)
  and is not used.

The full write has none of this (the target only ever changes by a rename), at the cost of
writing the whole file (up to about 32 MB of history).

### Reading

Reading never writes. A file is opened read-only and everything (identity, images, graph,
history) is read in **one read transaction**, so a commit by somebody else in between
cannot give a mixed state. A leftover `<name>-journal` that SQLite would have to replay
(an interrupted save) is **not** replayed by a read: loading fails with the message "the
project file has an unfinished save next to it (<name>-journal): open it from the computer
that saved it ...". `Project::recover_unfinished_save(path)` rolls the file back to its
last complete state; it is meant for an explicit user action on the computer that saved the
file, while no program is saving it. The next in-place commit or full write on the saving
computer also settles the journal. Locks are waited for at most 2 s per request (a save
asks twice); if the file is still locked the error is "The project file is in use by another
program or computer; try again." and nothing is changed or rewritten.

Deleted history entries leave free pages inside the file; they are reused by later
commits, but the file does not shrink until it is written in full (e.g. "save as").

Measured (dev build, local SSD): a project with 4,000 images and 100 history
entries of a 300 KB graph (a 31 MB file), one parameter change saved: full
rewrite about 60 ms and 32 MB written; in place about 30 ms and about 0.6 MB of
changed pages plus about the same in the journal. On a network share the written
volume matters far more than the time on a local disk (but see above: a network
share gets the full write).

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
  writes images, graph, folders and history in one transaction (in place, see "How a file is written"), increments the
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
not reused. For an in-place commit the check runs inside the write transaction, after the
write lock is taken: check and write are one atomic step, so of several commits from the same
base revision exactly one succeeds and the others get `Conflict` (tested with parallel
writers). The check and the swap of a full write (new file, other project, old schema) are
still separate steps; two of those at the same instant can both pass. After a `Conflict` the
caller reloads, or uses `commit_overwrite`.

An application that works on a copy of the project in the background (the folder scan) must
hand the commit state back: `Project::inherit_versioning` takes revision and history from the
original before the copy replaces it, otherwise a save made in the meantime looks like a
foreign change at the next save.

### Saving during a folder scan

The scan runs on a copy of the project. A save while it runs first takes the images
found so far (`Project::merge_found`) and the scanned folder into the project, so the
file holds what the user sees; entries whose hash or metadata is not read yet are stored
as pending (empty hash, `scan_pending` in `capture`) and are picked up by the next scan
of that folder. When the scan finishes, its copy inherits revision and history
(`Project::inherit_versioning`), so the next save is a normal commit, not a conflict.
Recovery copies written by autosave during a scan contain the images found so far too.

The images are merged into a copy of the session's project, and the copy replaces the project
only after the save succeeded: a failed save (conflict, I/O error, locked file) changes
nothing. `merge_found` never replaces an entry that has a hash with a half-finished one
(empty hash, or a pending capture over a read one): a save during a rescan keeps the good
hash and capture of files the rescan found changed; their stored size and time no longer
match the file, so the next scan reads them again. If the scan is cancelled after a save,
what was saved stays in the session and the file; entries that were still pending stay marked
pending and the next scan of the folder completes them. When the scan ends, the project counts
as unsaved only if the scan's result differs from what the session holds after taking over
the graph and the edits made meanwhile, so a scan that brings nothing new after the last
save leaves the project saved and creates no revision.

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
- A commit rewrites only the changed rows (see "How a file is written"); two people editing different images
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
