# Save patterns and the ignore list

Clean-room behavioural specification. It describes how Office, AutoCAD and Revit write files
next to the document, and which local names Synka leaves out of uploads (`src/sync/ignore.rs`).
Each rule states its public source. Where no public source was found, the rule says
`undocumented — review`. Those rules are Synka's own choices and need a check by someone who
has used the application.

## What applications do when they save

**Office (Word, Excel, PowerPoint).** While a document is open, Word writes an owner file
next to it. Its name is `~$` followed by the rest of the document name, and it stores the
user who opened the file. It is removed on a normal save and close. A crash can leave it
behind. Source: Microsoft Knowledge Base article 313472 (owner file naming); community
answers on Microsoft Q&A describe the same behaviour.

Office also writes temporary files during a save. Synka ignores names that are exactly eight
hexadecimal characters with no extension, and names starting `atmp`. The exact Office naming
is `undocumented — review`.

A save in Office is normally a write to a temporary file followed by a rename over the
original. Synka treats the result as a new version of the same item, not as a delete and a new
file. The sequence is `undocumented — review`. Synka's engine tests cover the resulting
version behaviour.

**AutoCAD.** Source: the Autodesk AutoCAD file-types article
(https://www.autodesk.com/blogs/autocad/autocad-file-types/). It is a blog post, not Help. Confirm
against the AutoCAD Help topics for the installed version.

- `.dwl` and `.dwl2`: lock files written next to an open drawing; they hold who has the drawing
  open. They normally disappear on close. Used by the WHOHAS command.
- `.bak`: backup written when a drawing is saved over an existing file (system variable
  ISAVEBAK, on by default).
- `.sv$`: autosave file, written at an interval (SAVETIME, default 10 minutes); it is removed on
  a clean exit and survives a crash.
- `.ac$`: an AutoSave variant. Source is the same blog post; `undocumented — review` beyond that.

**Revit.** Source: Autodesk Revit Troubleshooting, "Error: Data in File Is Corrupt"
(https://help.autodesk.com/cloudhelp/2023/ENU/Revit-Troubleshooting/files/GUID-03CABFE0-4A6B-4C1A-968F-57608192E96E.htm)
and Revit Help, "Backup and Journal Files" (https://help.autodesk.com/cloudhelp/2014/ENU/Revit/files/GUID-E79353E9-CC57-4ECE-93B8-5D12D9B3B135.htm).

- Each save keeps the previous version as `<stem>.NNNN.rvt` (four-digit counter); the highest
  number is the newest backup. Revit keeps a set number of backups and deletes the oldest.
- For a non-workshared model saved to a network share, Revit keeps up to three local copies in a
  Journal folder with timestamps in the names.
- `.slog`: `undocumented — review`. No public Autodesk source was found.
- `*_backup` folders (Revit central backups) and `.rvt_backup`: `undocumented — review`.

## The ignore list

Synka never uploads a name that matches one of these rules. Ignored local files are not uploaded. The rule set is in `src/sync/ignore.rs`.

| Rule | Examples | Public source |
|---|---|---|
| Office owner file prefix `~$` | `~$Ritning.docx` | Microsoft KB 313472 |
| LibreOffice owner prefix `.~lock.` | `.~lock.Budget.xlsx#` | `undocumented — review` (LibreOffice convention) |
| Office temp name, 8 hex characters, no extension | `A1B2C3D4` | `undocumented — review` |
| `atmp*` temp prefix | `atmp123.tmp` | `undocumented — review` |
| `.tmp`, `.temp`, `.swp`, `.lck` | `x.tmp` | `undocumented — review` (generic temp and lock extensions) |
| Partial downloads `.crdownload`, `.part`, `.partial` | `file.crdownload` | `undocumented — review` (browser partial-download extensions) |
| AutoCAD `.bak`, `.dwl`, `.dwl2`, `.sv$`, `.ac$` | `x.dwl2`, `x.sv$` | Autodesk AutoCAD file-types article (see above) |
| Revit journal `.slog` | `x.rvt.slog` | `undocumented — review` |
| Revit numbered backups `name.NNNN.rvt` / `.rfa` | `model.0001.rvt` | Autodesk Revit troubleshooting and Backup and Journal Files |
| Revit backup folders `*_backup`, `*.rvt_backup` | `Model_backup` | `undocumented — review` |
| Desktop metadata `desktop.ini`, `Thumbs.db`, `.DS_Store` | `Desktop.INI` | `undocumented — review` (Windows and macOS system files) |
| Shortcut files `.url`, `.lnk`, `.wsh` | `Länk till ACC.url` | Synka design rule (see below) |
| Hidden or temporary attributes (0x2, 0x100) | | Microsoft Learn, File Attribute Constants (`FILE_ATTRIBUTE_HIDDEN`, `FILE_ATTRIBUTE_TEMPORARY`) |

Name matching is ASCII case-insensitive. A rule applies to every path component, so a file
inside `Model_backup\` is ignored too.

### Why shortcut files are ignored

ACC listings do not show `.url`, `.lnk` or `.wsh` files. If Synka uploaded one, it would look
deleted in ACC and be removed locally on the next poll. Ignoring them keeps the local folder and
ACC consistent. This is an engine design decision, not a public rule.

## Browser partial files

Partial downloads (`.crdownload`, `.part`, `.partial`) are ignored so that a half-written file
is not uploaded. Once the browser renames the file to its final name, Synka sees an ordinary new
file.

## Review checklist for counsel and engineering

- Check every `undocumented — review` rule against the installed Office, AutoCAD and Revit
  versions.
- Keep the AutoCAD blog source out of any final product claim until it is checked against the
  AutoCAD Help topics.
