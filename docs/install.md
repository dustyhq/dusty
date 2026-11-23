# Installing Dusty

Every release ships the editor (`dusty`) and standalone Dusty Git (`dusty-git`) for Linux, macOS and Windows. The editor already has Dusty Git built in (`Space g g`), so you only need `dusty-git` to run it on its own.

## Linux

Download a binary or package from [releases](https://github.com/dustyhq/dusty/releases). For the static x86_64 binary:

```sh
mkdir -p ~/.local/bin
curl -fLo ~/.local/bin/dusty https://github.com/dustyhq/dusty/releases/latest/download/dusty-x86_64-linux
chmod +x ~/.local/bin/dusty
```

Install `dusty-git-x86_64-linux` as `~/.local/bin/dusty-git` the same way. Make sure `~/.local/bin` is on your `PATH`.

Debian, RPM, pacman, AppImage and Flatpak packages, and how to check the Sigstore-signed checksums, are in the [Linux release guide](release-linux.md).

## macOS

Download `dusty-aarch64-macos.tar.gz` for Apple Silicon or `dusty-x86_64-macos.tar.gz` for Intel from [releases](https://github.com/dustyhq/dusty/releases). Check the archive's SHA-256 against its entry in `SHA256SUMS`, then extract and install it:

```sh
tar -xzf dusty-aarch64-macos.tar.gz
mkdir -p ~/.local/bin
install -m 0755 dusty ~/.local/bin/
```

For standalone Dusty Git, do the same with `dusty-git-aarch64-macos.tar.gz` or `dusty-git-x86_64-macos.tar.gz`. Put `~/.local/bin` on `PATH`. These binaries are not Developer ID signed or notarized. If macOS quarantines the checked executable, run:

```sh
xattr -d com.apple.quarantine ~/.local/bin/dusty
```

The [macOS and Windows guide](user-guide-macos-windows.md) covers configuration paths and shell selection.

## Windows

Download `dusty-x86_64-windows.zip` from [releases](https://github.com/dustyhq/dusty/releases) and check its SHA-256 against its entry in `SHA256SUMS`. In PowerShell:

```powershell
Get-FileHash dusty-x86_64-windows.zip
Expand-Archive dusty-x86_64-windows.zip -DestinationPath "$env:LOCALAPPDATA\Programs\Dusty"
```

Add `%LOCALAPPDATA%\Programs\Dusty` to your user `PATH` and run `dusty` in Windows Terminal. `dusty-git-x86_64-windows.zip` holds standalone Dusty Git. The binary is not Authenticode signed, so SmartScreen may ask before the first run. The [macOS and Windows guide](user-guide-macos-windows.md) covers configuration paths, shells and what differs on Windows.

## From source

Use the pinned Rust toolchain plus a C compiler, `pkg-config`, OpenSSL and zlib development files, or enter the Nix development shell:

```sh
nix develop          # optional; provides the Linux build environment
cargo build --release --locked
./target/release/dusty
```

[CONTRIBUTING.md](../CONTRIBUTING.md) covers tests and plugin-fixture builds.

## First run

```sh
dusty                 # dashboard in the current directory
dusty path/to/file    # edit a file
dusty path/to/project # dashboard rooted at a directory
dusty --clean         # use bundled configuration
```

Only trust workspaces whose code you are willing to run. The first time you open a workspace, Dusty asks whether to trust it. Escape keeps editing without language servers, tasks or debuggers, and `:trust` approves it later. ACP agents are ordinary child processes and can run even in untrusted workspaces or safe mode.

On Linux, configuration lives in `~/.config/dusty/config.toml`, with `keymaps.toml` and `dashboard.toml` beside it. The bundled [settings](../assets/defaults/config.toml), [keymaps](../assets/defaults/keymaps.toml) and [dashboard actions](../assets/defaults/dashboard.toml) are the reference defaults. The [Linux user guide](user-guide-linux.md) covers XDG overrides, trust, recovery and daily workflows.

## Dusty Git sign-in

```sh
dusty-git                    # the repository in the current directory
dusty-git path/to/repo
dusty-git open https://github.com/owner/repo/pull/12   # a pull request, issue or run
```

Dusty Git signs in with what you already have: the forge's CLI (`gh`, `glab`, `fj` or `tea`) or a token variable like `GH_TOKEN`, `GITLAB_TOKEN` or `FORGEJO_TOKEN`. It never asks for a token and never shows one. Without a sign-in, public repositories open read-only, and `@` shows how to sign in. Self-hosted forges are detected from the remote, or you can name one in `config.toml`:

```toml
[forge.hosts."git.example.com"]
provider = "forgejo"   # github, gitlab, gitea or forgejo
```

`ui.git.keys = "lazygit"` switches to lazygit's keys. The [Linux user guide](user-guide-linux.md#forge-tabs) covers every sign-in, the cache and the keys.
