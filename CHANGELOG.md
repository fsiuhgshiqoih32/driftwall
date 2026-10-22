# Changelog

## 0.1.0

First release.

- Video wallpapers behind the desktop icons, decoded on the GPU through Media Foundation and
  scaled by the GPU into a swap chain per display. One decode per video, however many displays
  show it.
- Picture wallpapers, rendered at each display's exact size and set as the Windows desktop
  picture, so nothing runs for them.
- Per display, all displays, or one wallpaper spanning every display. Fit, Fill, Stretch and
  Center. Volume (videos start muted, and muted videos don't decode audio) and speed.
- Pauses while windows cover a display, while an app or game is full screen, on battery, in
  battery saver, while locked and while the screen is off. A video paused for 15 seconds is shut
  down completely and starts again when the desktop shows.
- The first frame of each video becomes the Windows desktop picture, so the desktop looks right
  before Driftwall starts and after it quits. The pictures Driftwall replaced come back when a
  display is cleared.
- Library window: add files with a dialog or by dragging files and folders; hard links on the
  same drive instead of copies; thumbnails; rename; remove.
- Tray icon, start with Windows, single instance, recovery after Explorer restarts, display
  changes, sleep and GPU resets.
- `--reset` to undo everything before deleting Driftwall; `--self-test`.
