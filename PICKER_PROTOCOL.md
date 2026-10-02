# Aurora Files picker

The same standalone `aurora-files` application supports file management and reusable file selection. Guest builds preserve the Wasm `posix_spawnp` PTY implementation and `/bin/sh` default. Normal Files starts with its existing terminal; picker mode never starts a shell or a media viewer.

## CLI

```
aurora-files --choose-file --path /home/admin/Documents --null
aurora-files --choose-files --title 'Upload files' --filter '*.png;*.jpg' --null
aurora-files --save-file --filename report.txt --path /home/admin/Documents --null
aurora-files --choose-directory --path /home/admin --null
```

`--path`, `--title`, `--filename`, and `--filter` each take one argument. Filters are semicolon-separated filename globs (`*`, `?`), matched case-insensitively; directories always remain navigable. The filter label can be clicked to toggle All files. Success writes absolute paths separated by NUL with `--null` (otherwise newline) and exits 0. Cancel exits 1. Save returns a destination without writing it; selecting an existing filename requires an additional Replace confirmation. Pass all arguments directly, without a shell.

## Resident service for Firefox and other X11 applications

Start one `aurora-files --picker-service` after X is ready. It remains hidden until requested and owns the X selection `_AURORA_FILE_PICKER_SERVICE`. Discover it with `XGetSelectionOwner`; zero means unavailable. The selection automatically disappears if the process exits.

1. Create a requester-owned X window on the same display.
2. Set `_AURORA_FILE_PICKER_REQUEST` on that window, type `UTF8_STRING`, format 8. Its value is these NUL-terminated UTF-8 fields, in order:
   `1`, mode (`open`, `multiple`, `save`, `directory`), initial directory, dialog title, default filename, semicolon-separated filter globs.
3. Send a ClientMessage directly to the selection-owner window with NoEventMask. Type `_AURORA_FILE_PICKER`, format 32, data[0] requester window XID, data[1] 1 (version), data[2] optional parent window XID for `WM_TRANSIENT_FOR`; data[3..4] zero.
4. Poll or monitor `_AURORA_FILE_PICKER_RESULT` on the requester window, type `UTF8_STRING`, format 8. NUL-terminated fields are `accept`, followed by one or more absolute paths; `cancel`; or `error`, followed by a message. A busy service returns `error` rather than mixing requests. The service never modifies the selected file.
5. Destroy the requester window after consuming a response. Destroying it while the dialog is open cancels the request and hides the picker. Also cancel if the service selection owner disappears.

Requests are bounded to 64 KiB; unsupported versions and malformed requests return errors. This protocol carries UTF-8 paths. CLI NUL output preserves Unix path bytes.

## Controls

- Double-click a directory to enter it; double-click a file or use Open to accept. Enter navigates directories/accepts files. Ctrl+Enter accepts a directory or current selection. Escape cancels.
- Multiple selection supports Ctrl-click, Shift-click, Ctrl+A, Space, and Shift+arrows. Arrow keys, Home/End, Page Up/Down move the selection.
- Ctrl+L or click the path to enter a location. Backspace goes up. Ctrl+H toggles hidden files; F5 refreshes. The picker toolbar supplies Home, Up, Sort, Hidden and New folder.
- Save mode has an editable name; Tab switches between list and name. Ctrl+A while editing clears the name.
- Normal Files adds Ctrl+Shift+N/new-folder menu, F2/rename menu, Ctrl+C/X/V copy/cut/paste. Copy is recursive and preserves symlinks; paste chooses a unique name and never silently replaces an existing item. Errors remain visible. Moves across filesystems report an error and retain the cut item.

Delete or Move to Trash opens a confirmation dialog. Accepted items are moved into the user's `Trash/files` directory with a unique name and a matching `Trash/info/*.trashinfo` record containing the original percent-encoded absolute path and deletion timestamp. The Trash sidebar exposes recoverable files; cut/paste moves them out again. There is no permanent-delete action. Moving across filesystems reports an error instead of deleting the source.
