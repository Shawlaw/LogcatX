# LogcatX clipboard helper

Android 10+ blocks ordinary background apps from reading the clipboard. The
shell UID is still allowed to use the clipboard service for testing, so LogcatX
ships a tiny (`~6 KB`) dex and runs it through `app_process` as `shell` on
`/data/local/tmp`. It does **not** install an APK, change the default IME, or
run in the background: LogcatX pushes it and invokes it only when the user
clicks a clipboard action.

## Protocol

`ClipboardBridge` writes one line to stdout:

| Line | Meaning |
| --- | --- |
| `CLIP <base64>` | Current text clipboard (UTF-8, base64) |
| `EMPTY` | The clipboard has no text |
| `ACK` | `set` succeeded |
| `ERROR <message>` | Failure |

Commands:

```text
app_process / com.logcatx.clipboard.ClipboardBridge get
app_process / com.logcatx.clipboard.ClipboardBridge set <base64>
app_process / com.logcatx.clipboard.ClipboardBridge set-stdin   # one base64 line on stdin
```

`set-stdin` is used for larger text so the base64 payload never has to fit in
the Windows command line.

## Rebuild

The compiled dex is committed because release/CI machines do not carry an
Android SDK. To rebuild after editing `ClipboardBridge.java`:

```powershell
./scripts/build_clipboard_helper.ps1
```

The script requires `javac` (JDK 8+) and an Android SDK with `platforms/` and
`build-tools/`; it uses `ANDROID_HOME` / `ANDROID_SDK_ROOT` when set. Commit both
the Java source and the regenerated `assets/clipboard-helper.dex`.

The `ActivityThread` / shell-context bootstrap follows the approach used by
scrcpy (Apache-2.0) for running framework code from `app_process`.
