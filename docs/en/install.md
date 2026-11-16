# Installation and run modes

[Documentation](README.md) · [中文](../zh/install.md)

## Packages and binaries

| What | Where | Use |
|---|---|---|
| Debian package | `make deb` (see [Building](#building)) | On PXVIRT / PVE nodes: tty1 console with the SDL graphical console |
| Static Linux binaries (musl) | [Releases](https://github.com/jiangcuo/pxvirt-console/releases): x86_64, aarch64, riscv64, loongarch64 | Client for `--ssh` / `--remote`, or quick use on a node without installing |
| macOS (arm64, x86_64) | Releases, zip files | Client for `--ssh` / `--remote` |

The macOS binary is not notarized. Remove the quarantine flag once after unpacking:

```bash
xattr -d com.apple.quarantine pxvirt-console
```

## Run modes

| Mode | Start | What works |
|---|---|---|
| Console | `pxvirt-console.service` on tty1 | Everything. Replaces the tty1 login; cannot be quit, only logged out (also after the idle timeout) |
| Local | `sudo pxvirt-console` in a terminal on the node | Everything; logs in as `root@pam` automatically |
| SSH | `pxvirt-console --ssh root@node[:port]` | Everything. One SSH master connection carries the API (port forward), host commands, shells, consoles and live metrics. SSH user `root` logs in as `root@pam` automatically |
| API | `pxvirt-console --remote node[:port]` | PVE API only. The certificate fingerprint is confirmed on first contact. Pages that need host commands (IO, RAID, connections, ...) are hidden |

On macOS and other non-Linux systems only `--ssh` and `--remote` are available.

### Saved connections

Every `--ssh` / `--remote` target is remembered in `~/.config/pxvirt-console/connections.json`. Started without a target
on a machine that is not a PVE node (or with `--connect`), the console shows the list: Enter connects, `a` adds, `e`
edits, `x` deletes. `--connect NAME` connects to a saved entry directly, `--list` prints them.

Enable the tty1 console after installing the package:

```bash
systemctl enable --now pxvirt-console.service
```

### Options

| Option | Meaning |
|---|---|
| `--ssh [user@]host[:port]` | SSH mode |
| `--remote host[:port]` | API mode |
| `--connect`, `-c [name]` | Saved connections: pick one from the list, or connect to `name` directly |
| `--list`, `-l` | Print the saved connections |
| `--lang en\|zh\|auto` | Interface language (tty1 is always English) |
| `--version` | Print the version |

## Configuration

`/etc/pxvirt-console/config.toml` (non-root users: `~/.config/pxvirt-console/config.toml`, which also stores known
certificate fingerprints). Example: [`debian/config.toml.example`](../../debian/config.toml.example).

| Key | Default | Meaning |
|---|---|---|
| `idle_timeout_secs` | 900 | Log out after this many seconds without input |
| `lang` | `auto` | `auto`, `en`, `zh` |
| `refresh_secs` | 5 | Minimum auto-refresh interval of a page |
| `prelogin_dashboard` | true | Host name, management URL and live metrics before login |
| `default_realm` | `pam` | Realm preselected on the login screen |
| `console_display` | `auto` | Graphical console: `auto` (SDL on tty1, terminal graphics elsewhere), `sdl`, `terminal` |
| `console_fps` | 10 | Frame limit of the terminal console renderer |
| `ssh_user` | `root` | User for host commands in SSH mode |
| `[health]` | | Thresholds of the health checks, see [Health checks](health.md#thresholds) |

## Recommended terminal

[Ghostty](https://ghostty.org) (Linux and macOS): Kitty graphics protocol and pixel-precise mouse reporting, so the
graphical VM console and the console previews are sharp. Kitty, WezTerm and Sixel terminals work too; other terminals
fall back to half-block characters (no previews).

## Building

```bash
cargo build --release                       # needs Rust and libsdl2-dev
make deb                                    # Debian package
```

Static build without SDL, as in CI (`pxvirt/musl` image, linkers in `.cargo/config.toml`):

```bash
cargo build --release --no-default-features --target x86_64-unknown-linux-musl
```

GitHub Actions builds every push (4 Linux architectures, 2 macOS) and publishes a release with the binaries and a
"What's Changed" list for every `v*` tag.
