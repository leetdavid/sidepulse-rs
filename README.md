# sidepulse (Rust)

A lean, unofficial replacement for the Python [SidePulse](https://github.com/inteliwear/sidepulse) companion on macOS.
It is not affiliated with inteliwear.
It drives SidePulse Pro/Dot LEDs from Claude Code, Codex, Cursor, Grok and Junie hooks, the OpenCode plugin
and the Herdr plugin, using one small binary with a native menu bar and settings window.

| | Python (installed build) | Rust |
|---|---|---|
| Processes | service + tray (+ eject guard) | one daemon with menu bar (+ eject guard) |
| Hook cost | new Python interpreter per event, ~147 ms | ~8 ms, no interpreter |
| Tray/daemon idle CPU | tray measured at ~90% | ~0% (event-driven, no 1 s tick) |
| Memory | tray 260 MB + service 55 MB | 13 MB footprint (≈37 MB after the settings window was opened) |
| Subprocesses | `ioreg` ×2 per second, `pmset` every minute | `sudo pmset disablesleep` only on transitions |
| Logs | unbounded (1.5 GB observed) | state changes only, capped at 2 MB |

## Commands

```sh
sidepulse install [--dry-run]     # copy binary, rewrite agent hooks, park Python agents, start LaunchAgent
sidepulse uninstall [--dry-run]   # restore Python hooks and agents byte-for-byte
sidepulse status [--json]         # sessions and devices as the daemon sees them
sidepulse write "off\n#ff00ff 1s pulse\nrepeat" [--device /Volumes/SidePulse]
sidepulse list [--json]
sidepulse pause | resume          # stop/restart automatic LED writes
sidepulse settings                # open the settings window
sidepulse hook --provider claude  # what agent hooks call (reads the payload on stdin)
sidepulse report --json           # what the OpenCode plugin calls (program_status JSON on stdin)
sidepulse herdr sync              # what the Herdr plugin calls on pane/agent events
sidepulse daemon [--no-menubar]   # what launchd runs
```

## How it works

- `hook` parses the payload, classifies it (same rules as upstream: 0 mismatches over 30,000 real
  Claude Code events), and sends one small JSON message to `~/.local/state/sidepulse-rs/daemon.sock`.
- The daemon keeps at most 64 sessions in memory (saved to `sessions.json`). The highest-priority session
  picks the LED animation: error > waiting > tool > working > done (20 min) > idle. Sessions expire after
  the configured timeout.
- LEDs are written only when the program changes. Every 60 s an uncached read of `LEDS.LED` keeps the SD
  reader awake and rewrites the program if the device reset.
- Lid open/close (IOKit clamshell notification), power changes (IOPS notification), and volume mounts
  (kqueue on `/Volumes`) are events, not polls.
- While agents work it holds a `PreventUserIdleSystemSleep` assertion and, if the upstream sudoers helper
  is installed, `pmset disablesleep 1` so a closed lid doesn't sleep the Mac. Both are released 5 minutes
  after agents finish, below the minimum battery level, or on exit.

## Menu bar and settings window

- The icon spins while agents work and pulses while one waits, using Core Animation (no app CPU per frame;
  static with Reduce Motion). The menu is built only when opened.
- Session rows show a status glyph and the app the session lives in. Click focuses it (Herdr pane, terminal,
  Claude/Codex app, VS Code, Cursor); Option-click resumes it in the configured terminal.
- Each device has a submenu for Agent Status / Battery Level / Manual and a brightness slider.
- Settings… opens a native window: per-state animation picker with live previews and "Show" on the device,
  an editor to create/edit/delete animations, general options, and integration status. Previews and validation
  run the firmware's own LED engine (`resources/sdled.wasm` from upstream) in-process via wasmi, so a preview
  and a parse error (with line and column) match what the device does. The window and its engines are freed on
  close; the preview timer only runs while it is open.

## Integrations

- **Herdr**: `install` links a Herdr plugin (`sidepulse.status`) that runs `sidepulse herdr sync` on agent
  events. Agents Herdr sees but hooks don't cover appear as sessions; hooked sessions get their pane attached so
  a click focuses it.
- **OpenCode**: `install` writes `~/.config/opencode/plugins/sidepulse.ts` (vendored from upstream), which
  reports through `sidepulse report --json`.

## Settings

The daemon reads the existing `~/.config/sidepulse/agent-monitor/settings.json` and `animations/*.LED`,
and reloads them when they change. Used keys: `agent_animations`, `custom_agent_animations`, `devices[]`
(`led_display`, `brightness`), `led_display`, `agent_list.*timeout*`, `sleep_prevention_policy`,
`sleep_prevention.min_battery_percent`, `battery_monitoring.*`, `lid_*_animation.duration_seconds`,
`session_open_preferences`, `session_terminal.app`, `show_menu_bar_icon`.

The settings window and menu bar write the same file; everything else can be edited by hand.

## Not ported

History charts and `status-history.jsonl`, the virtual Notch device, phone linking and relay, firmware
upgrades and USB-only Dot control (the Python CLI still works for these), animation profiles import/export,
T3 Code/Antigravity integrations, Codex plan-review detection, Linux/Windows. The native SD eject guard
from the Python install keeps running unchanged.

## Development

```sh
cargo test
cargo build --release
sh scripts/e2e.sh target/release/sidepulse ~/.config/sidepulse/agent-monitor/settings.json
```

`scripts/e2e.sh` runs a sandboxed daemon against a fake device; it never touches real hardware, power
settings or the installed services.

## License

MIT, see `LICENSE`. The bundled animations (`resources/animations`), LED engine (`resources/sdled.wasm`),
OpenCode plugin and Herdr plugin manifest come from [inteliwear/sidepulse](https://github.com/inteliwear/sidepulse)
under its MIT license (`resources/UPSTREAM_LICENSE`). The hook classification rules are ported from it.
