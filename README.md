# inno-updater

Windows background-update helper, used by Aira during an
in-place update. Inno Setup launches it to swap the installed executables once the running
application has exited.

## Relationship to upstream

A fork of **[microsoft/inno-updater](https://github.com/microsoft/inno-updater)** at version
**0.24.0**, MIT licensed. The upstream copyright notices and `LICENSE` are unchanged, and
the update logic is untouched.

## What differs

Upstream hardcodes its own product name in five Rust literals and five Windows resource
strings, because upstream *is* that product. Here the name is a **build parameter**, so a
rebranded build does not need a patched source tree:

| Variable | Default |
|---|---|
| `INNO_UPDATER_PRODUCT_NAME` | `Visual Studio Code` |
| `INNO_UPDATER_COMPANY_NAME` | `Microsoft Corporation` |

`build.rs` reads them, emits `INNO_UPDATER_PRODUCT_NAME` for `env!()`, and substitutes
`{{PRODUCT_NAME}}` / `{{COMPANY_NAME}}` in `resources/resources.rc.template`. Both default
to upstream's values, so a plain `cargo build` reproduces upstream behaviour exactly.

## Building

Inno Setup is 32-bit, so the helper must be `i686`:

```bash
rustup target add i686-pc-windows-msvc
INNO_UPDATER_PRODUCT_NAME="Your Product" \
  cargo build --release --target i686-pc-windows-msvc --bin inno_updater
```

Requires Windows with the MSVC toolchain. Note that **Smart App Control blocks this build**:
Cargo compiles unsigned build scripts and an Enforced policy refuses to execute them
(`os error 4551`). CI runners have no such policy.

## License

MIT — see [LICENSE](LICENSE). Copyright (c) Microsoft Corporation.

## Leftover processes (2026-10-31)

After the application's own executable has exited, the helper also ends any process still running
from the install folder (`process::capture_processes_under`): a language server or file search the
application started can outlive it, and a running executable cannot be deleted, which left an
installation half removed. The helper waits 2 s, then ends them; a failure there is logged and the
update continues.

## Recovering an interrupted update: `--recover` (2026-11-21)

```
inno_updater.exe --recover <install root> [<exe name> [<mutex name>]] [--from-setup]
```

Reads the install root for a background update that stopped half way (power loss, a killed
process, a crash) and repairs it when the files on disk prove which repair is safe. It never
shows a window or dialog and never touches anything outside the install root; the default
`<exe name>` is `<INNO_UPDATER_PRODUCT_NAME>.exe` and the default `<mutex name>` is the product
name in lower case without spaces (`aira` for Aira, matching `win32MutexName`). Pass the root as
the application reports its own folder (a long path, not an 8.3 short one).

### How the swap it recovers from works

Traditional layout (no `win32VersionedUpdate`, which is Aira's):

1. Setup stages the new version into `<root>\_`, in `[Files]` order: every file except the
   executable, then `<exe>`, its visual elements manifest, `bin\*`, and last
   `resources\app\product.json`. The installed version is untouched.
2. Only then does Setup start this helper (`ssPostInstall`, after `updating_version` is written
   and the application's mutex is gone). The helper waits for the application, ends programs
   still running from the folder, then **deletes** the old version -- every root entry except
   `_`, `tools`, `unins*`, `appx` and `bootstrap`, breadth first, root files first. There is no
   backup. Each file is renamed aside (`<name>.deleting-<pid>-<n>`) and marked for deletion; a
   folder that cannot be emptied stays and is merged into.
3. It moves each entry of `_` into the root, removes `_`, and rebases `unins000.dat`.

Versioned layout: Setup stages `new_<exe>` beside `<exe>`; the helper renames
`<exe> -> old_<exe>` (or `old_<n>_<exe>` when taken), then `new_<exe> -> <exe>`.

So in the traditional layout an update that has started deleting can only be finished, never
undone; "rolling back" there means discarding a stage that never finished, which is provable
only while the installed version is still complete.

### Decision table

The update moves `_` in directory order, which puts the executable first (`repair` always moves it
first), so "executable still staged, stage incomplete" is never a move in progress.

`overlap` = files present at the same relative path in `_` and in the root. Moving a file into
place removes it from `_`, so an overlapping root file was not put there by this update: it is an
old-version file deletion had not reached.

| Root `<exe>` | `_\<exe>` | `_` finished (`_\resources\app\product.json`) | Other evidence | Result |
|---|---|---|---|---|
| yes | -- (no `_`) | -- | no `new_<exe>` | **0** healthy, nothing changed |
| yes | -- (no `_`) | -- | `new_<exe>` | **0** versioned update staged, never applied; left alone |
| no | -- (no `_`) | -- | `new_<exe>` | **10** `new_<exe> -> <exe>` |
| no | -- (no `_`) | -- | exactly one `old_*<exe>` | **11** `old -> <exe>` |
| no | -- (no `_`) | -- | none, or several `old_*<exe>` | **20** refused |
| yes | yes | yes | root `resources\app\product.json` | **0** complete stage never applied (session ended); left for Setup |
| yes | yes | no | root `resources\app\product.json` | **11** staging stopped, updater never ran: `_` discarded |
| yes | yes | any | no root `resources\app\product.json` | **20** inconsistent |
| no | yes | yes | -- | **10** deletion was interrupted: delete the overlapping old files, move `_` in |
| no | yes | no | -- | **20** old exe gone and stage incomplete: reinstall |
| yes | no | any | no overlap, `product.json` in root or `_` | **10** move was interrupted after the exe: move the rest |
| yes | no | no | overlap, root `resources\app\product.json` | **11** staging stopped before the exe: `_` discarded |
| yes | no | yes | overlap | **20** inconsistent |
| yes | no | no | overlap, no root `resources\app\product.json` | **20** inconsistent |
| no | no | any | -- | **20** no executable anywhere |

Always refused (20): `_` is not a folder; `_` holds `tools`, `unins*`, `appx` or `bootstrap`; `_`
and `new_<exe>` both exist; a path is a file on one side and a folder (or link) on the other.

Neither the update nor `--recover` writes a journal or keeps a backup: every decision is made
from the files in the install root as they are, and the only record is the log file and the
stdout lines below.

A repair (10 or 11) is applied only when nothing is running: not while Setup's
`<mutex>setup` / `<mutex>-updating` mutex exists (session or `Global\`), not while any program
runs from the install root, and not while a needed name is held delete-pending by another
program. Otherwise the result is **21** and nothing changes.

Finishing (10, traditional) deletes only the overlapping old files, moves the executable first and
then every other entry of `_` (merging into surviving folders), removes `_`, and rebases
`unins000.dat` (a no-op for paths already rebased). Root files that are in neither the overlap nor
`_` are left: recover cannot tell an old-only file from a new file already moved, and never deletes
what it cannot attribute. Discarding (11) renames `_` aside in one step, then deletes it. Every
step is safe to repeat: a run that stops part way leaves a state that maps to the same action.

### `--from-setup`: when Setup is the caller

The running application cannot repair its own folder (a program running from the install root
always means 21), and after a broken swap the application may not start at all. So the caller is
Setup: `code.iss` runs `--recover <root> --from-setup` at the start of every install and update,
before it stages anything. At that point Setup already holds its own `<mutex>setup` and
`<mutex>-updating`, which without the flag would make every run report 21.

`--from-setup` skips **only** that mutex check. A program running from the install root, or a
file another program holds at a name the repair needs, still returns 21 and nothing changes.
Nothing else differs: the same decision table, exit codes, log file and stdout lines. Do not pass
it from anywhere but Setup; without it, a Setup mutex means another Setup is running and the
result is 21.

### Exit codes

| Code | Meaning |
|---|---|
| 0 | Nothing to do: the install is usable and nothing was changed |
| 10 | Completed: an interrupted update was finished |
| 11 | Rolled back: the previous version is in place (incomplete stage discarded, or `old_<exe>` restored) |
| 20 | Refused: the state does not prove any repair safe; nothing was changed |
| 21 | Busy: Setup, the updater or a program from the install folder is running, or a file is held; nothing was changed, try later |
| 30 | Error: bad arguments, or a repair failed part way; run `--recover` again to resume |

None of them is 1, which every other mode (and a helper too old to know `--recover`) exits with.
Each run logs to the same file as an update, `%TEMP%\vscode-inno-updater-<unix seconds>.log`, and
prints two lines to stdout: `recover: <result> (<code>): <reason>` and `log: <path>`.

### Capability probe: `--capabilities`

```
inno_updater.exe --capabilities
```

Prints one capability per line to stdout (today: `recover`) and exits 0. It opens no window,
writes no log file and reads nothing on disk. A helper without `--recover` prints `Bad usage` to
stderr and exits 1, so `exit 0 and a line equal to recover` is the test. The release binary is a
GUI-subsystem program: read its stdout through a pipe (as a child process), not in a console.
`--version` is unchanged (`Inno Update v<version>` on stderr).
