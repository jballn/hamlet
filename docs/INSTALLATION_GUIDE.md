# Hamlet Installation & Setup Guide

This guide covers the prerequisites, system configuration, environment setup, compilation, and operation for all tools in the **Hamlet** ecosystem:

- **`hamlet` CLI (`hamlet-cli`)**: Primary operator interface for village initialization, commissions, agent lifecycle, decrees, mail, and vault deliverables ([SPEC §12.1](../SPEC.md#121-cli-stage-1--with-daemon); [ADR-0042](adr/0042-cli-command-tree-and-tui-architecture.md)).
- **`hamlet-daemon`**: The singular governor daemon managing rootless Podman containers, UID allocation, POSIX ACL sandboxes, telemetry, the Mayor event loop, and the local HTTP/WS API ([SPEC §3](../SPEC.md#3-host-daemon-architecture-rust); [ADR-0041](adr/0041-daemon-local-api.md)).
- **`hamlet-tui`**: Interactive MUD-style terminal interface providing spatial navigation across village landmarks, live event streams, and status monitors ([SPEC §12.2](../SPEC.md#122-mud-style-tui-stage-2); [ADR-0043](adr/0043-tui-rooms-landmark-projection.md)).
- **`hamlet-web`**: 8-bit RPG pixel canvas frontend compiled to WebAssembly (WASM), served over HTTP by the daemon with real-time WebSocket telemetry ([SPEC §12.3](../SPEC.md#123-wasm-8-bit-rpg-frontend-stage-3); [ADR-0044](adr/0044-ws-protocol-and-web-arch.md)).

---

## Architecture & Operating Model

```
 ┌────────────────────────────────────────────────────────┐
 │ Operator Interfaces:                                   │
 │   • hamlet CLI                                         │
 │   • hamlet-tui (MUD Terminal)                          │
 │   • hamlet-web (8-Bit RPG Canvas in Browser)           │
 └───────────────────────────┬────────────────────────────┘
                             │ Local REST & WebSockets (Port 8765)
 ┌───────────────────────────▼────────────────────────────┐
 │ hamlet-daemon (Host Governor Daemon)                   │
 │   • Sled Telemetry DB      • Mail & Freight Router     │
 │   • Mayor Control Loop     • State Machine (charter)   │
 └───────────────────────────┬────────────────────────────┘
                             │ tokio process pipes / CLI exec
 ┌───────────────────────────▼────────────────────────────┐
 │ Rootless Podman Container Engine                       │
 │   • Subuid slots 1001-2000 (Immutable agent identities)│
 │   • Derived Alpine parcels + Pixi/NVM/Cargo toolchains │
 │   • POSIX ACL sandboxes in /var/freight/               │
 └────────────────────────────────────────────────────────┘
```

- **Linux**: Native host for the complete stack (daemon, rootless containers, subuid mapping, and all frontends).
- **Windows**: The daemon and container engine run inside **WSL2** (Linux kernel with subuid and cgroup support). The `hamlet` CLI and `hamlet-tui` can also be built and run natively on Windows to communicate with a local or remote daemon over HTTP/WS.
- **macOS**: The daemon and containers run inside a Linux virtual machine (**Podman Machine**, **Lima**, or **OrbStack**). The `hamlet` CLI and `hamlet-tui` compile natively on macOS to communicate with the daemon.

---

## 1. Prerequisites & Toolchain Requirements

### 1.1 Core Development Tools
- **Rust Toolchain**: Stable Rust 1.80 or later with `cargo` and `rustup`.
- **WebAssembly Target**: `wasm32-unknown-unknown` (required for building `hamlet-web`).
- **Git**: Version 2.30+ for commission repository tracking and submodule handling.
- **POSIX ACL Tools**: `setfacl` and `getfacl` (from package `acl`) for freight sandbox verification.
- **Build Utilities**: C compiler (`gcc` or `clang`), `pkg-config`, `openssl` headers, and `xxd` (for WASM magic byte validation during web packaging).

### 1.2 Container Runtime (Daemon Host)
- **Podman**: Version 4.5+ or 5.x configured for rootless execution.
- **Shadow Utilities**: `newuidmap` and `newgidmap` with `setuid` or `cap_setuid=ep` / `cap_setgid=ep` file capabilities.
- **Subuid/Subgid Allocation**: At least 65,536 subordinate UIDs and GIDs mapped to the daemon user in `/etc/subuid` and `/etc/subgid`.
- **Systemd Lingering**: Enabled for the user running the daemon (`loginctl enable-linger <user>`).
- **Disk-Backed Work Area**: A non-volatile directory with at least 4 GiB free space on an ext4, xfs, or btrfs partition (e.g. `/var/hamlet/tmp`). Container image pulls and multi-GB toolchain extractions **must not** run on `tmpfs` or `ramfs`.

---

## 2. Platform Setup Guides

### 2.1 Linux

#### A. Debian / Ubuntu / Pop!_OS / Linux Mint (`.deb`)

1. **Install system dependencies**:
   ```bash
   sudo apt-get update
   sudo apt-get install -y \
     build-essential \
     pkg-config \
     libssl-dev \
     curl \
     git \
     acl \
     xxd \
     podman \
     uidmap \
     slirp4netns
   ```

2. **Configure subordinate UID and GID ranges** ([ADR-0011](adr/0011-host-permission-model.md)):
   Ensure your user has allocated subuids and subgids in `/etc/subuid` and `/etc/subgid`. Replace `$USER` with your username:
   ```bash
   # Check existing allocation
   grep "^$USER:" /etc/subuid || sudo usermod --add-subuids 100000-165535 "$USER"
   grep "^$USER:" /etc/subgid || sudo usermod --add-subgids 100000-165535 "$USER"
   ```

3. **Verify setuid / capability status on `newuidmap` and `newgidmap`**:
   ```bash
   ls -l /usr/bin/newuidmap /usr/bin/newgidmap
   # If setuid is missing and getcap shows no caps, enable setuid:
   # sudo chmod u+s /usr/bin/newuidmap /usr/bin/newgidmap
   ```

4. **Enable systemd user lingering**:
   Rootless container scopes require lingering so the user cgroup remains available when logged out or running headless:
   ```bash
   loginctl enable-linger "$USER"
   ```

5. **Establish disk-backed work directory**:
   ```bash
   sudo mkdir -p /var/hamlet/tmp
   sudo chown -R "$USER:$USER" /var/hamlet/tmp
   export TMPDIR=/var/hamlet/tmp
   ```

---

#### B. Fedora / RHEL / CentOS Stream / Rocky / AlmaLinux (`.rpm`)

1. **Install system dependencies**:
   ```bash
   sudo dnf install -y \
     gcc \
     gcc-c++ \
     make \
     pkgconf-pkg-config \
     openssl-devel \
     curl \
     git \
     acl \
     vim-common \
     podman \
     shadow-utils \
     slirp4netns
   ```

2. **Configure subuids and subgids**:
   ```bash
   grep "^$USER:" /etc/subuid || sudo usermod --add-subuids 100000-165535 "$USER"
   grep "^$USER:" /etc/subgid || sudo usermod --add-subgids 100000-165535 "$USER"
   ```

3. **Enable systemd lingering**:
   ```bash
   loginctl enable-linger "$USER"
   ```

4. **Establish work directory**:
   ```bash
   sudo mkdir -p /var/hamlet/tmp
   sudo chown -R "$USER:$USER" /var/hamlet/tmp
   export TMPDIR=/var/hamlet/tmp
   ```

---

#### C. Other Linux Distributions (Arch Linux, openSUSE, Alpine, Void)

- **Arch Linux**:
  ```bash
  sudo pacman -S --needed base-devel git curl podman shadow acl openssl xxd
  sudo usermod --add-subuids 100000-165535 "$USER"
  sudo usermod --add-subgids 100000-165535 "$USER"
  loginctl enable-linger "$USER"
  ```
- **openSUSE (Tumbleweed / Leap)**:
  ```bash
  sudo zypper install -y gcc gcc-c++ make pkg-config libopenssl-devel git curl podman shadow acl xxd
  sudo usermod --add-subuids 100000-165535 "$USER"
  sudo usermod --add-subgids 100000-165535 "$USER"
  loginctl enable-linger "$USER"
  ```

#### D. Verifying the Linux Host with the Doctor Tool

Hamlet includes an automated host doctor check in [`scripts/guarded-service-user.sh`](../scripts/guarded-service-user.sh):

```bash
env TMPDIR=/var/hamlet/tmp ./scripts/guarded-service-user.sh --doctor
```

Expected diagnostic output:
```
== identity
  user                               dock (uid 1000, groups: dock wheel)
  HOME / TMPDIR                      /home/dock / /var/hamlet/tmp
== userns / subuid (QA04-07)
  ok   subuid range                  dock:100000:65536
  ok   subgid range                  dock:100000:65536
  ok   newuidmap                     setuid present
  ok   newgidmap                     setuid present
  ok   max_user_namespaces           1048576
== subuid reachability
  ok   unshare chown -> subuid       inside 1000 became host uid 100999
== cgroups (podman container creation)
  ok   podman info                   usable
  ok   systemd lingering             enabled for dock
== work area (QA04-08)
  ok   work area fs                  /var/hamlet/tmp is ext4
  ok   work area free                84 GiB
```

---

### 2.2 Windows

Because Hamlet utilizes Linux user namespaces, `podman unshare`, and POSIX file permissions for agent containment ([SPEC §4](../SPEC.md#4-container--security-model)), the engine operates through **WSL2** (Windows Subsystem for Linux), with native client tooling available on Windows.

#### Option A: Full Engine via WSL2 (Recommended)

1. **Install WSL2 with Ubuntu**:
   In PowerShell (as Administrator):
   ```powershell
   wsl --install -d Ubuntu-24.04
   ```
   Restart your computer when prompted.

2. **Configure WSL2 Systemd Support**:
   Inside your Ubuntu WSL terminal, ensure `/etc/wsl.conf` has systemd enabled:
   ```ini
   [boot]
   systemd=true
   ```
   Restart WSL from PowerShell with `wsl --shutdown` and reopen Ubuntu.

3. **Follow the Debian/Ubuntu Linux instructions**:
   Follow Section 2.1(A) above inside your WSL terminal to install `podman`, `subuid`, `subgid`, and Rust.

4. **Accessing the Web GUI from Windows**:
   WSL2 automatically bridges localhost. Once `hamlet-daemon` is running in WSL on port 8765, open any Windows browser (Chrome, Edge, Firefox) and navigate to:
   ```
   http://localhost:8765/
   ```

---

#### Option B: Native Windows Client Tools (`hamlet` CLI & `hamlet-tui`)

You can compile and run `hamlet` CLI and `hamlet-tui` directly in Windows Command Prompt, PowerShell, or Windows Terminal, connecting to a daemon running in WSL2 or a remote Linux host.

1. **Install Prerequisites**:
   - Install **Visual Studio Community** with the *"Desktop development with C++"* workload (or Visual Studio Build Tools).
   - Install Rust via [rustup-init.exe](https://rustup.rs/).
   - Install **Git for Windows**.
   - Install **Windows Terminal** (recommended for TrueColor and ANSI terminal support in `hamlet-tui`).

2. **Compile Native Windows Binaries**:
   In PowerShell:
   ```powershell
   git clone https://github.com/jballn/hamlet.git
   cd hamlet
   cargo build --release -p hamlet-cli -p hamlet-tui
   ```
   Binaries will be placed in `target\release\hamlet.exe` and `target\release\hamlet-tui.exe`.

3. **Connect to Remote / WSL Daemon**:
   ```powershell
   .\target\release\hamlet.exe --endpoint 127.0.0.1:8765 --token <token-from-wsl> daemon status
   .\target\release\hamlet-tui.exe --endpoint 127.0.0.1:8765 --token <token-from-wsl>
   ```

---

### 2.3 macOS

macOS uses Darwin, which does not provide native Linux containers or Linux subuid namespaces. The `hamlet-daemon` and containers run inside a Linux VM (via Podman Machine or Lima), while `hamlet` CLI and `hamlet-tui` compile natively on macOS.

#### Option A: Running Daemon & Containers via Podman Machine

1. **Install Homebrew packages**:
   ```bash
   brew install podman rustup-init pkg-config openssl git
   rustup-init -y
   source "$HOME/.cargo/env"
   ```

2. **Initialize and start a Podman rootless virtual machine**:
   ```bash
   # Create podman machine with adequate disk and CPU
   podman machine init --cpus 4 --memory 4096 --disk-size 50 hamlet-vm
   podman machine start hamlet-vm
   ```

3. **Verify rootless subuid mapping inside the VM**:
   ```bash
   podman machine ssh hamlet-vm "cat /etc/subuid && podman info"
   ```

4. **Port Forwarding for Web & API**:
   Podman machine exposes port 8765 to your macOS localhost automatically when containers or host services bind to 0.0.0.0.

---

#### Option B: Native macOS Client Tools (`hamlet` CLI & `hamlet-tui`)

Compile the client binaries natively on macOS (Apple Silicon or Intel):

1. **Build CLI and TUI**:
   ```bash
   git clone https://github.com/jballn/hamlet.git
   cd hamlet
   cargo build --release -p hamlet-cli -p hamlet-tui
   ```

2. **Run Native TUI**:
   Use **Terminal.app**, **iTerm2**, **Alacritty**, or **Ghostty**:
   ```bash
   ./target/release/hamlet tui --demo
   ```
   To connect to a live daemon:
   ```bash
   ./target/release/hamlet-tui --endpoint 127.0.0.1:8765 --token <daemon-token>
   ```

---

## 3. Building Hamlet from Source

### 3.1 Install Rust & WASM Target

```bash
# Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"

# Add WebAssembly compilation target
rustup target add wasm32-unknown-unknown
```

### 3.2 Clone the Repository

```bash
git clone https://github.com/jballn/hamlet.git
cd hamlet
```

### 3.3 Compile Native Binaries

Compile all workspace crates in release mode:

```bash
cargo build --release --workspace --exclude hamlet-web
```

This compiles:
- `target/release/hamlet` (CLI)
- `target/release/hamlet-daemon` (Daemon binary)
- `target/release/hamlet-tui` (MUD TUI standalone binary)

### 3.4 Build the Web Frontend Bundle (`hamlet-web`)

The WebAssembly 8-bit RPG frontend is compiled and packaged using [`scripts/build-web.sh`](../scripts/build-web.sh):

```bash
./scripts/build-web.sh
```

This script:
1. Compiles `crates/hamlet-web` to `wasm32-unknown-unknown` in release mode.
2. Validates the WebAssembly binary header magic (`0061736d`).
3. Bundles HTML, JS, CSS, sprite sheets, and audio files into `crates/hamlet-web/dist/`.
4. Computes a cryptographic `manifest.json` sha256 asset hash for client cache negotiation.

### 3.5 Install to User PATH

Install the CLI and TUI binaries into `~/.cargo/bin`:

```bash
cargo install --path crates/hamlet-cli
cargo install --path crates/hamlet-tui
```

Verify that `hamlet` is accessible:
```bash
hamlet --version
hamlet --help
```

### 3.6 Shell Completions

Generate shell completions for your shell:

```bash
# Bash
hamlet completions bash > ~/.local/share/bash-completion/completions/hamlet

# Zsh
hamlet completions zsh > ~/.zfunc/_hamlet

# Fish
hamlet completions fish > ~/.config/fish/completions/hamlet.fish
```

---

## 4. Configuration & Environment Variables

### 4.1 Village Root (`.hamlet/`)

Hamlet operates on a **file-as-truth** architecture ([SPEC §1](../SPEC.md#1-concept--pillars)). All state lives in the `.hamlet/` directory of your village tree.

Initialize a new village:
```bash
# In an empty directory or existing project
mkdir -p my-village && cd my-village
hamlet init .
```

This generates the normative structure:
```
.hamlet/
├── charter.json       # Canonical state machine
├── daemon.endpoint    # Written by daemon when running (host:port)
├── daemon.pid         # Daemon PID
├── daemon.token       # Local auth token (mode 0600)
├── guild/             # Blueprints, registered agents, commissions
├── courier/           # Mail spools and freight sandboxes
├── keep/              # Agent homes and census records
└── tavern/            # Gossip context and telemetry
```

### 4.2 Environment Variables

| Variable | Default | Purpose |
|---|---|---|
| `HAMLET_DIR` | Current directory or ancestor `.hamlet` | Root village directory path. |
| `HAMLET_DAEMON_PORT` | `8765` | TCP port for daemon REST and WebSocket API. |
| `HAMLET_DAEMON_BIND` | `127.0.0.1` | Bind address for daemon server. |
| `HAMLET_AUTH_TOKEN` | Stored in `.hamlet/daemon.token` | Authentication token for local API calls. |
| `HAMLET_WEB_DIR` | `crates/hamlet-web/dist` | Directory holding static web bundle assets. |
| `TMPDIR` / `HAMLET_WORK_AREA` | `/tmp` | Disk-backed scratch directory for Podman builds and toolchains. |
| `HAMLET_PODMAN_TESTS` | `0` | Set to `1` to run rootless podman integration tests. |

---

## 5. Running the Hamlet Tools

### 5.1 Starting and Stopping the Daemon

```bash
# Run daemon in foreground (for logging and development)
hamlet daemon run

# Or start daemon as a background service
hamlet daemon start

# Query daemon heartbeat status
hamlet daemon status

# Stop daemon cleanly
hamlet daemon stop
```

When started, the daemon logs its local API endpoint (e.g. `127.0.0.1:8765`) and creates `.hamlet/daemon.token` (permissions `0600`).

### 5.2 Operating Commissions and Agents via CLI

```bash
# Create a new software engineering commission
hamlet commission new --slug feature-auth --prompt "Implement JWT auth in Rust"

# List tracked commissions
hamlet commission list

# View commission details
hamlet commission show feature-auth

# Inspect running or hibernated agents
hamlet agent list

# Send and read courier mail
hamlet mail send --to 1001 --subject "Spec check" --body "Please review SPEC §4"
hamlet mail read --slot 1001

# Inspect Mayor decrees and human escalations
hamlet decree list

# Export deliverables from the vault
hamlet vault list
hamlet vault export --slug feature-auth --out ./dist
```

### 5.3 Launching the MUD Terminal Interface (`hamlet-tui`)

```bash
# Launch interactive terminal UI connected to the local village daemon
hamlet tui

# Or start in a specific village room (guild, tavern, courier, market, keep, manor)
hamlet tui --room manor

# Run standalone demo mode (simulated telemetry, no live daemon required)
hamlet tui --demo
```

Key Navigation in `hamlet-tui`:
- `Tab` / `Shift+Tab`: Cycle between room landmarks.
- `1` through `6`: Jump directly to Guild, Tavern, Courier, Market, Keep, or Manor.
- `Esc` / `q`: Exit TUI.

### 5.4 Launching the 8-Bit Web Interface (`hamlet-web`)

1. Ensure the web assets are built:
   ```bash
   ./scripts/build-web.sh
   ```
2. Start the daemon:
   ```bash
   hamlet daemon run
   ```
3. Open your browser:
   ```
   http://127.0.0.1:8765/
   ```

Features available in the browser:
- Live 2D RPG village canvas with animated sprites for agents and Mayor.
- Landmark interaction (click buildings to inspect ledger, spools, and blueprints).
- Real-time token consumption, latency, and cost telemetry via WebSockets.
- Over-the-head agent conversation speech bubbles.

---

## 6. Verification & Self-Testing

To ensure the host environment meets all security, purity, and isolation requirements, run Hamlet's verification suite:

```bash
# Run full local CI battery (formatting, clippy, unit tests, e2e scenarios)
./scripts/ci-local.sh

# Run multi-cycle village soak marathon
./scripts/soak-village.sh
```

---

## 7. Troubleshooting

### `newuidmap: write to uid_map failed: Operation not permitted`
- Root cause: `/usr/bin/newuidmap` lacks setuid permissions or file capabilities, or the user is missing from `/etc/subuid`.
- Solution: Run `sudo usermod --add-subuids 100000-165535 "$USER"` and `sudo chmod u+s /usr/bin/newuidmap /usr/bin/newgidmap`.

### `work area fs is tmpfs — RAM-backed; point TMPDIR at disk`
- Root cause: Podman container build floors extract ~1.5 GB toolchains and root filesystems; building inside a small RAM-backed `/tmp` causes disk-space exhaustion or OOM failures.
- Solution: Point `TMPDIR` to an ext4/xfs path:
  ```bash
  export TMPDIR=/var/hamlet/tmp
  ```

### `Failed to connect to bus: No such file or directory` (systemd user session)
- Root cause: Running under a headless session without an active systemd user bus.
- Solution: Enable user lingering with `sudo loginctl enable-linger "$USER"`. A full desktop session bus is not required when lingering is active.

### `address already in use (os error 98)` on port 8765
- Root cause: Another daemon instance is already bound to port 8765.
- Solution: Check running instances with `hamlet daemon status` or `lsof -i :8765` and stop it via `hamlet daemon stop` or `kill $(cat .hamlet/daemon.pid)`.
