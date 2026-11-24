# Packaging notes

## Run

```powershell
powershell -ExecutionPolicy Bypass -File D:\Claude\rawgraph\packaging\package.ps1
# options: -OutDir <folder>   (default %LOCALAPPDATA%\rawgraph\dist)
#          -SkipBuild          (reuse the exe in %LOCALAPPDATA%\rawgraph\target-dist)
#          -RetryMinutes <n>   (retry a failing build every 60 s)
```

Result: `<OutDir>\RawGraph-<version>-windows-x64\` and the matching `.zip`. Only that
one folder (name must match `RawGraph-*`, directly inside OutDir) is replaced.

## What it does and checks

- Builds `rawgraph-app` in release with `RUSTFLAGS=-C target-feature=+crt-static` into
  `%LOCALAPPDATA%\rawgraph\target-dist` (set for this build only), `-j 8`.
- `dumpbin /dependents`: fails if the exe imports VCRUNTIME*, MSVCP*, api-ms-win-crt-*,
  ucrtbase.dll. Warning only if dumpbin is not found.
- `cargo metadata`: dependency closure of `rawgraph-app` over normal and build edges
  (no dev); every non-workspace package gets its LICENSE/LICENCE/COPYING/NOTICE/UNLICENSE
  files copied to `LICENSES\<name>-<version>\`.
- `THIRD-PARTY-NOTICES.txt`: name, version, licence expression, repository; plus the list of
  packages without a licence file. For those, standard texts are copied (unchanged) from other
  packages on disk into `LICENSES\_standard\`, see `SOURCES.txt` there. No text is invented.
- Summary with counts per licence expression; GPL (not LGPL), AGPL, SSPL or missing
  expression is printed as `NEEDS REVIEW`.

- Before zipping, staged files with a LastWriteTime before 1980-01-01 or in the future (some
  cargo-registry licence files) get the current time, because ZIP cannot store such dates
  (Compress-Archive failed with "DateTimeOffset ... ZIP-Datei-Timestamp"). Only the copies in
  the stage folder are changed, never the registry sources.

## Not covered

- No installer (zip of a folder only).
- No code signing: SmartScreen will warn.
- `rawler` is LGPL-2.1 and is statically linked into the exe. Static linking carries
  relinking obligations (the recipient must be able to replace the library and relink).
  This package answers that only with a source offer in README.txt. The owner must decide
  (dynamic split, object files, or replacing the library) before any sale or wider distribution.
- Licence expressions come from crate metadata; they were not legally reviewed. Notice
  files that crates ship only in their repository (not in the published package) are missing.
- RawGraph's own licence is undecided; README says "all rights reserved".
- Not tested on a second machine (only on the build machine, where the VC runtime is installed;
  the dumpbin check is the evidence for independence from it). GPU variety on other PCs is untested.
