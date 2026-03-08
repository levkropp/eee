# eee

Hold **Ctrl+Alt+Shift+E** for 10 seconds to restart `explorer.exe`.

![screenshot](screenshot.png)

A tiny (~250KB) Windows utility that runs silently in the background. When you hold down the hotkey combo, a countdown overlay appears. Release early to cancel, or hold to completion to kill and restart Explorer.

## Install

Download `eee.exe` from [Releases](https://github.com/levkropp/eee/releases), then run:

```
eee.exe install
```

This copies itself to `%LOCALAPPDATA%\eee\` and creates a scheduled task that starts at logon. You'll never need to think about it again.

## Uninstall

```
eee.exe uninstall
```

Removes the scheduled task and deletes installed files.

## Features

- Single-instance (duplicate launches silently exit)
- Runs at startup via Windows Task Scheduler
- No tray icon, no window, completely invisible until triggered
- Windows 11-styled overlay with progress bar
- Works on Windows 10 and 11

## Building

```
cargo build --release
```

Output: `target/release/eee.exe`

## License

GPL-3.0
