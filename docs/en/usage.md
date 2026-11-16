# Using the console

[Documentation](README.md) · [中文](../zh/usage.md)

## Screen layout

```
 PXVIRT │ node · cluster                          SSH / remote   user   node arch   time
  F1 System   F2 Network   F3 Disks   F4 Services & Logs   F5 PVE        ← modules
  1 Overview  2 Health  3 Memory  ...                                    ← pages
 ┌ list ───────────────────────────┐ ┌ details ───────────────┐ ┌ live ┐ (wide terminals)
 │                                 │ │                        │ │      │
 └─────────────────────────────────┘ └────────────────────────┘ └──────┘
 Actions  e Edit  x Delete  ...                                          ← every action of the page
 CPU  Memory  Network  Disk                      ◷ tasks  ● alerts       ← live status
 Esc back  Enter menu  / filter  F6 node  ? help  Ctrl+Q quit            ← hints
```

The action bar lists every action of the selected row with its key; dimmed actions are not possible right now
(the menu shows why).

## Keys

| Key | Action |
|---|---|
| F1 – F5 | Modules |
| 1 – 9, `[` `]` | Pages of the module |
| Tab, ← → (h l) | Move between list and details |
| ↑ ↓ (j k), PgUp PgDn, Home End (G) | Move in lists |
| Enter, `m` | Action menu of the selection |
| letters | Actions directly (see the action bar); UPPERCASE = forced / dangerous variant |
| Space | Mark rows for batch actions |
| `/` | Filter the list (Esc clears) |
| Esc | Back: details → filter → page sub-view (configuration page, file browser) → marks |
| `r`, Ctrl+R | Refresh now (also while paused: one snapshot) |
| F7 | Pause / resume all refreshing, live graphs and the status bar included |
| F6 | Switch node |
| F8 | Tasks |
| F10 | Shell on the node |
| `g` | Global menu: switch node, pause, task center, alerts (health page), language, shell, about, log out |
| `?` | Help |
| Ctrl+Q | Quit (asks; disabled on the system console) |

Graphical VM console: Ctrl+] opens its menu in a terminal, Ctrl+Alt+Shift+Q on tty1 (send keys, change display,
close).

## Mouse

Click module and page tabs, action buttons, the alert counter (opens the health page), rows (double or right click opens the menu) and the console preview of a
VM. The wheel scrolls lists and details.

## Pausing

F7 freezes everything: page reloads, the live sampler (IO, traffic, status bar) and the [health checks](health.md). The header shows
`⏸ paused · F7`. `r` takes one new snapshot while paused.

## Login and permissions

| Started as | Login |
|---|---|
| tty1 / serial console | PVE login screen (anyone can walk up to the console) |
| root in a terminal on the node (`sudo pxvirt-console`) | automatic, as `root@pam` |
| `--ssh root@node` | automatic, as `root@pam` |
| `--ssh user@node` (not root), `--remote`, non-root terminal | PVE login screen |

The process itself runs as root on the console (like getty or a display manager); the login decides what the person
in front of it may do. In a terminal or over SSH as root the operating system has already authenticated the operator,
so the console logs in as `root@pam` without asking. Automatic sessions have no login screen to return to: log out,
the idle timeout and leaving the shell quit the program. The audit log records the origin
(`login (terminal, sudo by lucas, automatic)`).

On the login screen you log in with a PVE account (TFA supported). Actions are shown according to the account's PVE privileges; the menu
explains missing ones. Host actions (RAID, connections, services, ...) need `root@pam` or the matching node privilege
and local / SSH access. If the PVE API is down, `root@pam` can log in with the local password into a rescue mode
(PVE via `pvesh`, host tools only).

Every write action is logged to the journal of the node (`journalctl -t pxvirt-console`).

## Languages

English and Chinese (`--lang`, or the global menu). tty1 always uses English because the Linux console has no CJK font.
