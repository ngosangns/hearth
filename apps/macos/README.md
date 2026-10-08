# Hearth macOS app

NativePHP Desktop v2 shell for the Hearth workspace UI. The Rust `hearth` binary is copied into `extras/hearth` at build time and is not committed.

The window reads and writes `~/Library/Application Support/HearthApp/workspaces.json` in the same shape as `hearth tui`: `{ id, path, trusted, addedAt }`, uppercase ids, `addedAt` without fractional seconds. A file that does not decode is moved aside. Selecting a folder runs `manager status`. Trust (second press) and Start run `manager ensure` and keep the bearer token in process memory (a SysV segment, because `php -S` drops statics between requests). Refresh never ensures. Stop (second press) leaves the daemon stopped until Start. Forget removes the row and does not stop the daemon.

Once a daemon is attached, the window polls `GET /v1/services`, shows groups from `groupTree`, URLs, and the daemon log. Start, stop, and restart wait on the operation. Reclaiming an unowned port takes two presses and is the only request that sends `killUnowned`. Shared recipes come from `hearth shared list`. Instances come from `hearth shared status`, or from `hearth shared installed` when smp is down. Drawing the shared pane does not start smp.

`/spike` is the earlier bundle check. The first open points `~/.local/bin/hearth` at the bundled binary when that symlink is missing, already points inside an `.app`, or points at an older `task install` build. A newer install, a regular file, and any other symlink are left alone. The versioned file is never deleted.

```sh
sh scripts/copy-hearth.sh
php artisan test
php artisan native:build mac arm64 --no-interaction
sh scripts/check-bundle.sh
```

`HEARTH_BIN` overrides the source binary. `php artisan native:run` opens the window against `extras/hearth` in the project tree. `HEARTH_WORKSPACE_FILE` overrides the workspace list for a scratch session. The built app is `nativephp/electron/dist`. The last built `.app` is the spike bundle until the next `native:build`.

On the first arm64 build the binary landed at `Hearth.app/Contents/extras/hearth`, outside `app.asar`. electron-builder ad-hoc signed it. `spctl` rejects that bundle until it is signed with a Developer ID and notarized.

This app does not supervise processes. It shells out to the bundled `hearth` binary. Developer ID signing and notarization need `NATIVEPHP_APPLE_ID`, `NATIVEPHP_APPLE_ID_PASS`, and `NATIVEPHP_APPLE_TEAM_ID` at build time. Without them the `.app` runs on this Mac only.
