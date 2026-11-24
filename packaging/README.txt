RawGraph (prototype)
====================

RawGraph is a local-first RAW photo editor. You build non-destructive edits as
a graph of nodes (tone, colour, detail, geometry, masks, filters) and see the
result live, with scopes (histogram, waveform, parade, vectorscope) and export.

THIS IS A PROTOTYPE. Expect rough edges, missing features and bugs.

System requirements
-------------------
- Windows 10 or Windows 11, 64-bit
- A graphics card with current drivers (DirectX 12 or Vulkan). If the GPU
  cannot be used, RawGraph falls back to calculating the preview on the CPU.
- Nothing else to install (no Visual C++ runtime, no .NET).

First steps
-----------
1. Start RawGraph.exe (no installation; run it from any folder you can write
   to, or copy the folder anywhere).
2. Open a folder that contains RAW files (or JPEG/PNG/TIFF) with the folder
   command of the application. The folder is scanned including sub-folders.
3. Select an image, edit it with the node graph and the inspector, export.

Your original files are never modified. Edits are stored as parameters in a
project file that you save yourself; exports are written only to the folder
you choose.

Where RawGraph keeps its data
-----------------------------
Recovery copies of your work, the thumbnail cache and saved recipes are kept in

    %LOCALAPPDATA%\rawgraph

(normally C:\Users\<your name>\AppData\Local\rawgraph). You can delete that
folder at any time; it only holds caches and recovery copies. Window position
and view settings are stored by the user-interface library in the usual
per-user application data folder (%APPDATA%). Nothing is sent over the network.

Windows SmartScreen warning
---------------------------
RawGraph.exe is not digitally signed, so Windows SmartScreen will probably show
"Windows protected your PC". To start the program anyway, click "More info",
then "Run anyway". If the file was downloaded or received as a zip, you can
also right-click the zip before extracting, choose Properties, and tick
"Unblock".

Licences
--------
RawGraph itself: all rights reserved, private prototype, do not redistribute.

RawGraph contains third-party open-source components. Their names, versions and
licences are listed in THIRD-PARTY-NOTICES.txt; the licence texts are in the
LICENSES folder (one sub-folder per component).

The RAW decoding library "rawler" (version 0.8.0) is licensed under the
GNU LGPL, version 2.1. Its source code is available from its repository:
https://github.com/dnglab/dnglab
The complete corresponding source of this build of RawGraph is available on
request from the person who gave you this program.
