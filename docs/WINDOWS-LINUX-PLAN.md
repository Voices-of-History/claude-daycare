# Windows and Linux support for daycare-runner — plan

**Summary**

1. Linux is close. On Ubuntu 24.04 (x86_64) the runner builds, all 293 tests pass, pairing works against the mock platform, and the usage meter drives a pty through util-linux `script` (tested with a fake `claude`). What is missing: a real credential store (today it falls back to a 0600 file and prints "macOS keychain"), a safer default workspace root, keep-awake, an installer that accepts Linux, and Linux release artifacts. About **4–6 days**.
2. Windows does not compile. There are 22 errors from a cross-check, all in a few places: file locks, Unix permissions, the pid check, and `localtime_r`. The larger work is behavior: no `script(1)` (the meter needs ConPTY), `HOME` is unset, the default workspace sits inside the user profile, owner-only checks are no-ops, registry policy is not checked, and the test harness is built on `#!/bin/sh` fakes. About **12–15 days** for native Windows.
3. "WSL only" is a good first Windows step. It is the Linux build plus WSL detection, a check of the Windows-side Claude policy, and caveats about host sleep. About **1–2 days** on top of Linux. OpenCode's own docs recommend WSL, and Codex and Claude Code both run there.
4. The seal holds on Linux as it does on macOS, with one weakness (a shared `/tmp` root). On native Windows it has four gaps that need new code: workspace location, ACL checks, registry policy, and `.cmd` shims. None is a reason to skip Windows, but all four must be fixed before a native Windows release.
5. Order: Linux (x86_64 and arm64, static musl), then CI and a per-target release manifest, then WSL, then native Windows only if people ask for it. Linux first is confirmed cheaper.

Written 2026-09-28 from `main` @ 66d0178 (the same tree as `fix-usage-meter-0927`). Plan only; no feature code on this branch. Codex and OpenCode adapter design belongs to `docs/MULTI-AGENT-PLAN.md` on `multi-agent-plan-0928`. This doc covers only where the platform changes their picture.

Tags: **[V]** verified on this box or in code · **[D]** from vendor docs (linked) · **[A]** assumption, not yet tested.

---

## 1. What I ran

| Experiment | Result |
|---|---|
| `cargo build` on Ubuntu 24.04 x86_64, Rust 1.94.1 (pinned) | OK in 12 s [V] |
| `cargo test` | **293 passed, 0 failed** (lib 196, bin 19, child_env 1, cli_end_to_end 47, platform_client 6, turn_runner 24) [V] |
| `cargo build --release --target x86_64-unknown-linux-musl` (zig as the C compiler for `ring`) | Static-pie ELF, 4.3 MB, runs [V] |
| `enroll` against `dev/mock-platform.py` with the **default** token store (no `DAYCARE_TOKEN_FILE`) | Pairs. `/usr/bin/security` is missing, so every write prints `!! keychain write failed … No such file or directory` and the token lands in `tokens.json` (0600). The enroll summary says `stored in macOS keychain (service claude-daycare) (fallback: …)` [V] |
| `usage --claude-bin <fake>` through util-linux `script` 2.39.3 | Works: the child gets `/dev/pts/N`, argv arrives intact (including the empty `--setting-sources ""`), `/usage` and `/exit` arrive as keystrokes, and the reading is parsed. 2.3 s [V] |
| Default workspace root with `TMPDIR` unset | `/tmp/claude-daycare-josh/<actor>`, dirs 0700 [V] |
| Secret Service on this headless box | No `DBUS_SESSION_BUS_ADDRESS`, no `XDG_RUNTIME_DIR`, no `secret-tool`; `busctl --user` fails [V]. Headless servers are exactly where a Linux visit is likely to run. |
| `cargo check --target x86_64-pc-windows-gnu --all-targets` (zig cc for `ring`) | 3 lib errors, then (with those stubbed in a throwaway copy) 19 bin errors and 2 test errors [V]. See §3.1. |

No real `claude` model turn or `/usage` screen was run, and nothing touched production. The mock platform ran on 127.0.0.1 only.

---

## 2. Inventory of macOS-specific assumptions

| # | Area | Where | macOS today | Linux | Windows |
|---|---|---|---|---|---|
| 1 | Credential store | `src/keychain.rs:62` (`/usr/bin/security`), `:95` `MacKeychain`, `:177` `default_store` | Keychain via `security(1)` with a 10 s deadline and a file fallback | `security` missing, so the file fallback is used every time and a failure warning prints on every write [V] | Same fallback. `0o600` is ignored (`src/paths.rs:225`) [V] |
| 2 | Store wording | `src/keychain.rs:142,267`; `src/main.rs:848,1006,3677` | "macOS keychain" | Misleading on Linux [V] | Misleading |
| 3 | Config root | `src/paths.rs:54` needs `HOME` | `~/.claude-daycare` | OK | `HOME` is normally unset in PowerShell/CMD, so the runner fails with "HOME is not set" [A: Git Bash sets it] |
| 4 | Workspace root | `src/paths.rs:169-173`: `temp_dir()/claude-daycare-$USER` | `$TMPDIR` is per-user and 0700 | `/tmp` is shared (1777). Another account can squat the name; the runner then fails closed (chmod EPERM or the owner check). `$USER` may be unset under cron or systemd. `systemd-tmpfiles` ages `/tmp` [A: default 10 d on Ubuntu] | `%TEMP%` is **inside** `%USERPROFILE%`, which breaks the "not under home" rule in `src/paths.rs:17-35`. `guard_ancestors` refuses if `~\.claude\CLAUDE.md` exists, and otherwise the profile is still an ancestor |
| 5 | Owner-only checks | `src/workspace.rs:475-499` (uid + mode), `src/paths.rs:218-228`, `src/turn.rs:437-447` | Enforced | Enforced [V] | `cfg(not(unix))` checks only `is_file`/`is_dir`. **The seal is silently weaker** |
| 6 | Managed policy guard | `src/workspace.rs:214-270` | `/Library/Application Support/ClaudeCode`, two managed plists | `/etc/claude-code` ✓ [D] | `C:\Program Files\ClaudeCode` ✓ [D], but **HKLM/HKCU `SOFTWARE\Policies\ClaudeCode` is not checked** [D][V] |
| 7 | Usage meter pty | `src/usage_meter.rs:848` (`/usr/bin/script`), `:989-1024` | BSD `script -q -F` | util-linux branch already exists and works [V]. BusyBox `script` (Alpine) differs [A] | No `script(1)`. Needs ConPTY |
| 8 | Meter clock | `src/usage_meter.rs:248-261` `localtime_r`, `tm_gmtoff` | libc | libc ✓ | Does not compile [V] |
| 9 | Claude config path | `src/usage_meter.rs:614-631`, `src/workspace.rs:67-71`, `src/main.rs:3388` use `HOME` | `~/.claude.json` | ✓ | `%USERPROFILE%\.claude.json` [D]; needs `USERPROFILE` |
| 10 | Keep-awake | `src/keep_awake.rs:13,37`: `/usr/bin/caffeinate -i -s -w`, macOS-only | Holds idle sleep | No-op. Laptops will idle-sleep mid-visit | No-op |
| 11 | Detach | `src/main.rs:1843-1858`: `setsid` in `pre_exec` | Survives SIGHUP | ✓. Logind `KillUserProcesses=yes` would kill it at logout [A: Ubuntu default is `no`; this box has `Linger=yes`] | `cfg(unix)` only. Needs `DETACHED_PROCESS \| CREATE_NEW_PROCESS_GROUP` |
| 12 | SIGINT | `src/main.rs:1573-1586`: `libc::signal(SIGINT)` | ✓ | ✓ | Compiles (msvcrt `signal`) but should be `SetConsoleCtrlHandler`, or the `ctrlc` crate [A] |
| 13 | Locks | `src/main.rs:2105-2165`: `flock`, `fcntl(FD_CLOEXEC)` so the homecoming child inherits the lock (`:2986,3082`) | ✓ | ✓ | Does not compile. Needs `LockFileEx` plus `SetHandleInformation(HANDLE_FLAG_INHERIT)` |
| 14 | Pid liveness | `src/visit.rs:1068` `kill(pid,0)` | ✓ | ✓ | Does not compile. Needs `OpenProcess` + `GetExitCodeProcess` |
| 15 | Imports | `src/main.rs:50-51` `std::os::fd`, `std::os::unix::fs::PermissionsExt` unconditional | ✓ | ✓ | Does not compile |
| 16 | Hostname | `src/main.rs:875-882` `/bin/hostname` | ✓ | Usually present; missing on some minimal images, where the name falls back to none [A] | Missing. Use `COMPUTERNAME` or `GetComputerNameW` |
| 17 | `claude` lookup | `Command::new("claude")` `src/main.rs:561,584` | ✓ | ✓ | std finds only `claude.exe` on PATH, so an npm `claude.cmd` shim is "not installed". Running a `.cmd` also re-parses the inline JSON args through cmd.exe [A] |
| 18 | Human hints | `src/paths.rs:8-14` POSIX `shell_quote`; `src/config.rs:67-72`, `src/main.rs:3568` print `cd '…' && claude --resume` | ✓ | ✓ | Wrong quoting for PowerShell |
| 19 | Canonical paths | `src/launch.rs:399` `canonicalize` feeds `--mcp-config` and `--append-system-prompt-file` | ✓ | ✓ | Returns `\\?\C:\…` verbatim paths [A: whether `claude.exe` accepts them]. Use `dunce` |
| 20 | Tests | `tests/support/mod.rs:197,264,354` write `#!/bin/sh` fake `claude`s; `tests/cli_end_to_end.rs:3383` uses `libc::kill` | ✓ | ✓ | Most CLI and turn tests cannot run |
| 21 | Toolchain | `rust-toolchain.toml` `targets = ["aarch64-apple-darwin"]` | ✓ | Host builds are fine; add cross targets | Add targets |
| 22 | Release check | `dev/release-check.sh` builds only `aarch64-apple-darwin` | ✓ | — | — |
| 23 | Installer | `claudedaycare.com/install.sh` (= platform `public/install.sh`) lines 21-27 refuse anything but `Darwin-arm64`; needs `shasum`; PATH hint edits `~/.zshrc` | ✓ | Refused. `shasum` is often missing (use `sha256sum`). Bash users need `~/.bashrc` | Refused, and there is no PowerShell installer |
| 24 | Signing | Release binary has no Developer ID signature (no `Developer ID` string in the Mach-O) [V] | Works because `curl` downloads carry no quarantine flag [A] | Not needed; sha pin is enough | Unsigned `.exe` risks SmartScreen or Defender prompts [A] |
| 25 | Release pin | platform `src/lib/daycare/runnerRelease.ts:19` `CURRENT_RUNNER_RELEASE = "b5433d3bf"`; runner `src/wire.rs:134` `option_env!("DAYCARE_RUNNER_RELEASE")`; `src/main.rs:666-686` checks `releases/current.txt` | One id, one binary | OK if every target is built from the same commit and stamped with the same id | Same |
| 26 | Release pipeline | Binaries are committed into platform `public/releases/`. The comment names `tools/daycare-runner/dev/publish-release.sh`, which exists in **neither** repo [V]. Runner repo has no CI (`.github` absent) [V] | Manual | — | — |

---

## 3. Linux

### 3.1 What breaks today

Nothing fails to compile or test [V]. These are the problems in actual behavior:

1. **Credential store** (inventory #1–2). Each write tries `/usr/bin/security`, fails, and warns loudly. `status` then reports "FILE FALLBACK — the keychain refused a write". The token is safe (0600 under a 0700 root) but the messages are wrong.
2. **Workspace root in `/tmp`** (#4). It works on a single-user box. On a shared box another account can deny service, though never compromise the seal, because every path fails closed.
3. **Keep-awake is a no-op** (#10). The README's sleep promise is macOS-only.
4. **Installer refuses Linux**, and there are no Linux artifacts (#23, #26).

### 3.2 What replaces the keychain

| Option | Works headless | Survives reboot | Deps | Verdict |
|---|---|---|---|---|
| Secret Service via `secret-tool` (libsecret-tools), run through the existing `run_with_deadline` | No (needs a D-Bus session and an unlocked collection) | Yes | CLI only; mirrors `MacKeychain` | **Use on desktops** |
| Secret Service via the `keyring` crate (v4.2: `zbus-secret-service-keyring-store`, `linux-keyutils-keyring-store`, `windows-native-keyring-store`, `apple-native-keyring-store`) [D: docs.rs/keyring] | Same | Same | zbus plus async deps; the crate today has 6 direct deps | Viable; one crate would also cover Windows (§4.2) |
| Kernel keyutils (user or persistent keyring) | Yes | **No**; persistent keyrings expire [A: 3 d default] | `keyutils` | Reject: losing the device token means re-pairing |
| 0600 file under the 0700 root (exists: `FileTokenStore`) | Yes | Yes | none | **Use on headless machines** |

Recommendation: `LinuxStore = SecretToolStore` when `DBUS_SESSION_BUS_ADDRESS` is set and `secret-tool` exists, and otherwise `FileTokenStore` as the chosen store rather than as a failure fallback. For the wording, compare Claude Code itself: on Linux it keeps its own OAuth login in `~/.claude/.credentials.json` at 0600 [D: code.claude.com/docs/en/authentication]. The Daycare token at 0600 therefore adds no new exposure class. `status` should say "file (0600), the same place Claude Code keeps its own login on Linux", with no `!!` alarm.

Keep the 10 s deadline. A locked GNOME keyring can raise a prompt that no one can answer, the same failure the macOS code already handles (`src/keychain.rs:10-15`).

### 3.3 Workspace root

Default to `/tmp/claude-daycare-<uid>`, using the numeric uid rather than `$USER`. Before use, `lstat` the root and refuse unless it is a real directory, owned by the euid, mode 0700.

Rejected alternative: `$XDG_RUNTIME_DIR`. It is 0700, but it is torn down when the user's last session ends unless lingering is enabled, and that would pull the cwd out from under a detached visit [A]. Keep `DAYCARE_WORKSPACE_ROOT` as the override.

### 3.4 Keep-awake and detach

- Keep-awake: `systemd-inhibit --what=idle:sleep --who=claude-daycare --why=visit --mode=block tail --pid=<runner> -f /dev/null`, run through the same `KeepAwake` guard. Skip it quietly when `systemd-inhibit` is missing (containers, WSL) [A: inhibitor allowed for an active session by default polkit].
- Detach: `setsid` is enough on Ubuntu. Document `loginctl enable-linger` for boxes with `KillUserProcesses=yes` [A].

### 3.5 Artifacts and installer

- Targets: `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl`. They are static, have no glibc floor, and suit Alpine and old distros. `ureq` uses rustls and `ring`, so there is no OpenSSL; the x86_64 musl build works [V].
- `install.sh`: branch on `uname -s`/`-m` for `Darwin-arm64`, `Linux-x86_64` and `Linux-aarch64`; use `sha256sum` or `shasum -a 256`, whichever exists; pick the PATH hint from `$SHELL`; detect WSL (§5). The install location stays `~/.local/bin`, which is also where Claude Code installs itself [D].

### 3.6 Agents on Linux

| Agent | Linux status | Notes |
|---|---|---|
| Claude Code | First-class; Ubuntu 20.04+, Debian 10+, Alpine 3.19+ [D: code.claude.com/docs/en/setup] | Login lives in `.credentials.json`, so headless turns and the meter need no keychain [D] |
| Codex CLI | First-class [D] | The multi-agent plan's app-server meter (`account/rateLimits/read`) needs no pty, which is simpler than Claude's meter |
| OpenCode | First-class | Seal is config and permissions only; no OS dependency |

---

## 4. Windows (native)

### 4.1 Compile errors ([V], cross-check against `x86_64-pc-windows-gnu`)

| File:line | Error | Fix |
|---|---|---|
| `src/usage_meter.rs:253,258` | `localtime_r`, `tm_gmtoff` | `GetTimeZoneInformationForYear`, or `chrono` with `clock` |
| `src/visit.rs:1070` | `libc::kill` | `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` + `GetExitCodeProcess == STILL_ACTIVE` |
| `src/main.rs:50-51` | `std::os::fd`, `std::os::unix` | cfg-gate |
| `src/main.rs:2119-2160` (15 errors) | `flock`, `fcntl`, `FD_CLOEXEC`, `as_raw_fd`, `from_mode` | a `platform::lock` module: `LockFileEx` / `SetHandleInformation` on Windows, the current code on Unix |
| `tests/cli_end_to_end.rs:3383` | `libc::kill(SIGKILL)` | `Child::kill` or `TerminateProcess` |

This is only the compile floor. Several `cfg(not(unix))` branches compile today but silently drop a guarantee (inventory #5).

### 4.2 Credentials

Use Windows Credential Manager (DPAPI-backed generic credentials), through the `keyring` crate's `windows-native-keyring-store` [D] or `CredWriteW`/`CredReadW` via `windows-sys`. Claude Code itself keeps its login in `%USERPROFILE%\.claude\.credentials.json`, protected by profile ACLs [D]. The file fallback is therefore acceptable, but Credential Manager is cheap and has no prompt-hang risk.

### 4.3 Usage meter via ConPTY

- Replace `/usr/bin/script` with an in-process pty: `portable-pty` (from WezTerm) provides ConPTY on Windows and openpty on Unix [A: crate choice]. The runner reads the master itself, which removes the capture-file polling. The same code could replace `script(1)` on macOS and Linux, giving one code path.
- Risk: ConPTY re-renders its own buffer. It sends absolute cursor moves, full repaints, and scrolls at the bottom row. `src/terminal.rs` handles CUP/ED/EL/ECH/DCH but has "no auto-wrap and no scroll-off" (`src/terminal.rs:13-14`). **A real Windows `/usage` capture fixture is required** before trusting the meter; plan for emulator additions (scroll regions `r`, `S`/`T`).
- Codex's meter (app-server JSON-RPC) and OpenCode's (none) need no pty. Only the Claude meter carries this cost.

### 4.4 Paths, shells, processes

- Home: use `std::env::home_dir()` (returns `USERPROFILE` on Windows since Rust 1.85 [A]) everywhere `HOME` is read (#3, #9).
- Workspace root must be **outside** `%USERPROFILE%`. Proposal: `C:\ProgramData\ClaudeDaycare\<user-SID>\`, created with a protected DACL granting only the user SID and SYSTEM, and a check that the DACL still has that shape before each turn. This is a design decision for Josh (§8 Q2).
- Strip `\\?\` from canonical paths (`dunce`) before they reach `claude` argv (#19).
- Resolve `claude` with PATHEXT and prefer `claude.exe`. Refuse `.cmd`/`.bat` shims, or run them only after checking that std's batch escaping accepts the args (Rust ≥1.77.2 errors on unescapable batch args [A]) (#17).
- Detach with `creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW)`. Keep-awake with `SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED)` held by the visit process. Ctrl-C with `SetConsoleCtrlHandler`.
- Homecoming lock inheritance: std passes `bInheritHandles=TRUE`, so marking the lock handle inheritable works. It also leaks into any child spawned at the same time; accept this (only the homecoming child runs then) or use `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` [A].
- Human hints: print PowerShell-quoted commands on Windows (#18).

### 4.5 Agents on Windows

| Agent | Native Windows | WSL |
|---|---|---|
| Claude Code | Supported: Windows 10 1809+ / Server 2019+; `irm https://claude.ai/install.ps1 \| iex`; Git for Windows optional; Claude's own sandboxing not supported natively [D: code.claude.com/docs/en/setup]. Daycare does not use Claude's sandbox (it removes tools), so this does not matter. | Supported; the Linux install inside the distro, with a separate login [D] |
| Codex CLI | Native installer plus an AppContainer/restricted-token sandbox, still labeled experimental [D: [Codex KB](https://codex.danielvaughan.com/2026/04/01/codex-cli-windows-native-sandbox-wsl/), [ITECS guide](https://itecsonline.com/post/how-to-install-codex-cli-on-windows-2026-guide)]. The multi-agent plan's `--sandbox read-only` seal would rest on that experimental sandbox. | Linux-grade Landlock/seccomp [D] |
| OpenCode | Runs, but the vendor recommends WSL [D: [opencode.ai/docs/windows-wsl](https://opencode.ai/docs/windows-wsl/)] | Recommended |

### 4.6 Signing

- Windows: Azure Artifact Signing (formerly Trusted Signing), $9.99/month, paid Azure subscription, individuals in US/Canada eligible [D: [pricing](https://azure.microsoft.com/en-us/pricing/details/artifact-signing/), [FAQ](https://learn.microsoft.com/en-us/azure/artifact-signing/faq)]. **This costs money; it is Josh's decision.** Unsigned is workable for a friends beta: `Invoke-WebRequest` downloads run from PowerShell do not get SmartScreen's Explorer prompt [A], but Defender may still flag an unknown binary [A].
- macOS: unchanged (not Developer-ID signed today [V]).
- Linux: the sha pin, as today. Claude Code does the same for its Linux binaries [D].

---

## 5. WSL as the first Windows step

It is reasonable. WSL2 runs the Linux musl binary unchanged, and all three agents run there, one of them by vendor recommendation. What WSL needs beyond Linux:

| Item | Work | Tag |
|---|---|---|
| Detect WSL (`/proc/sys/fs/binfmt_misc/WSLInterop` or `WSL_DISTRO_NAME`) in `install.sh` and `status` | small | [A] |
| Policy guard: Claude on WSL can inherit Windows policy (`wslInheritsWindowsSettings` in HKLM or `C:\Program Files\ClaudeCode`) [D: managed-settings]. Also check `/mnt/c/Program Files/ClaudeCode/` and `reg.exe query HKLM\SOFTWARE\Policies\ClaudeCode` through interop; refuse if either can't be read | 0.5 d | [D] |
| Host sleep: WSL cannot hold the Windows host awake. Say so in `visit start` output. Optional: `powershell.exe` interop calling `SetThreadExecutionState` | doc / 0.5 d | [A] |
| Distro lifetime: confirm a `setsid` visit survives closing every WSL terminal (the WSL idle shutdown behavior has changed across versions) | test | [A] |
| Claude must be installed and logged in **inside** WSL (separate from any Windows `claude`) | installer message | [D] |
| Secret Service is absent in WSL, so it uses the file store | none | [A] |

Downside: people must have WSL, which excludes some. Upside: nearly all of the Linux work is reused, and the native-Windows seal gaps (§6) disappear.

---

## 6. The seal on each platform

| Mechanism | Where | macOS | Linux | Windows native |
|---|---|---|---|---|
| Workspace outside home | `src/paths.rs:17-35,169` | ✓ | ✓ (`/tmp`; add owner check §3.3) | **Gap**: `%TEMP%` is under the profile; needs a new root (§4.4) |
| No ancestor CLAUDE.md or rules | `src/workspace.rs:395-446` | ✓ | ✓ [V: tests pass] | Path logic ports; needs verbatim-path handling [A] |
| Owner-only files and dirs | `src/workspace.rs:475-499` | ✓ | ✓ | **Gap**: `is_file` only; needs a DACL check (owner = current SID, no other ACEs besides SYSTEM/Administrators) |
| Symlink refusal | `src/workspace.rs:341-393,448-462` | ✓ | ✓ | `symlink_metadata` sees symlinks; junctions [A: std reports name-surrogate reparse points as symlinks] |
| Env scrub (`ANTHROPIC_*`, `CLAUDECODE`, …) | `src/launch.rs:85-91`, `tests/child_env.rs` | ✓ | ✓ [V] | `env_remove` is case-insensitive on Windows [A]; needs a Windows run of `child_env` |
| Tool set (`--tools`, `--strict-mcp-config`, `dontAsk`, `--disallowedTools`) | `src/launch.rs:177-240` | ✓ | ✓ (same CLI) | Same flags [A]; empty-string args need std's quoting, which a `.cmd` shim breaks (#17) |
| Post-hoc `system/init` check | `src/stream.rs:366` | ✓ | ✓ | ✓ (platform-neutral) |
| Managed-policy refusal | `src/workspace.rs:64-270` | files + plists | `/etc/claude-code` ✓ | **Gap**: add `HKLM`/`HKCU\SOFTWARE\Policies\ClaudeCode` [D] |
| Personal-subscription check | `src/workspace.rs:79-120` | ✓ | ✓ | ✓ |
| Token only in child env, never argv or disk | `src/turn.rs:150-161`, `src/workspace.rs:310-316` | ✓ | ✓ (`/proc/<pid>/environ` is same-uid only; same threat scope as `src/workspace.rs:20-24`) | ✓ (same-user readable; same scope) |
| Token at rest | §3.2, §4.2 | Keychain | Secret Service or 0600 file | Credential Manager |

Codex's seal adds an OS sandbox (`--sandbox read-only`). It is Landlock/seccomp on Linux and WSL, and experimental on native Windows [D]. OpenCode's seal is config only, so it is the same everywhere.

---

## 7. CI, release, installer

1. **CI (new, runner repo).** GitHub Actions matrix: `macos-14` (aarch64-apple-darwin), `ubuntu-24.04` (x86_64 gnu tests + musl build), `ubuntu-24.04-arm` (aarch64 musl), and later `windows-2022` (x86_64-pc-windows-msvc). Steps: `cargo fmt --check`, `cargo test --locked`, release build with `DAYCARE_RUNNER_RELEASE=$(git rev-parse --short=9 HEAD)`. Runners for public repos are free [A].
2. **One release id for all targets.** Keep `CURRENT_RUNNER_RELEASE` as one string. Every artifact in a release comes from the same commit and carries the same `DAYCARE_RUNNER_RELEASE`, so the 426 floor (`runnerRelease.ts:25-38`) needs no change.
3. **Per-target manifest.** Replace the single URL and sha in `install.sh` with `releases/current.json`:
   `{"release":"<id>","targets":{"aarch64-apple-darwin":{"url","sha256"},"x86_64-unknown-linux-musl":{…},"aarch64-unknown-linux-musl":{…},"x86_64-pc-windows-msvc":{…}}}`.
   The multi-agent plan's self-update (§5.3 there) proposes a `current.json` with one url and sha. **Make it per-target from day one; this is the one place the two plans must agree.**
4. **Hosting.** Binaries sit in the platform's git `public/releases/` (3.6 MB each). With 4–5 targets per release, move them to GitHub Releases on the public runner repo or to blob storage, and have `current.json` point there [A: Josh's call].
5. **Publish script.** Write the missing `publish-release.sh`: build the matrix (or fetch CI artifacts), then write `current.json`, `current.txt`, `install.sh` pins, `install.ps1` pins and `runnerRelease.ts` in one platform commit.
6. **Windows installer.** `irm https://claudedaycare.com/install.ps1 | iex`. It checks `$env:PROCESSOR_ARCHITECTURE` and downloads to `$env:USERPROFILE\.local\bin\daycare-runner.exe` (the same directory Claude Code uses [D]). It verifies with `Get-FileHash -Algorithm SHA256`, adds the directory to the user PATH through `[Environment]::SetEnvironmentVariable(…,'User')`, and prints the enroll step. Self-update on Windows cannot overwrite a running `.exe`: rename it to `.old`, then move the new one in.

---

## 8. Size and order

| Step | Piece | Estimate |
|---|---|---|
| **L1** | Linux credential store (`secret-tool` + file mode) and honest wording | 1 d |
| L2 | Workspace root: uid, `lstat` owner and mode check | 0.5 d |
| L3 | Keep-awake via `systemd-inhibit`; README Linux notes | 0.5 d |
| L4 | Small fixes: hostname via `gethostname`, PATH hint, `release-check.sh` targets | 0.5 d |
| L5 | `install.sh` per OS/arch, `sha256sum`, WSL detection | 0.5 d |
| L6 | CI matrix, musl builds, `current.json`, publish script, hosting move | 1–1.5 d |
| L7 | Live check on Linux with real `claude` (`dev/visit-check.sh` on a subscription — Josh approves the spend) | 0.5 d |
| | **Linux total** | **4.5–5 d** |
| **S1** | WSL: Windows-side policy check, host-sleep message, lifetime test on a real WSL box | 1–1.5 d |
| **W1** | Compile floor: `platform` module for locks, pid, localtime, perms | 1.5 d |
| W2 | Paths: `home_dir`, SID workspace root with DACL, `dunce`, PATHEXT/`.cmd` handling, PowerShell hints | 1.5–2 d |
| W3 | Credential Manager store | 0.5–1 d |
| W4 | Detach, Ctrl-C, keep-awake | 1 d |
| W5 | ConPTY meter (`portable-pty`), Windows capture fixture, emulator scroll support | 2–3 d |
| W6 | Seal: registry policy, DACL checks for scaffold files and workspace | 1–1.5 d |
| W7 | Test harness: replace `#!/bin/sh` fakes with a Rust fake-`claude` binary (also helps the Codex and OpenCode fakes) | 2 d |
| W8 | `install.ps1`, Windows CI, signing setup (if bought) | 1–1.5 d |
| W9 | Live validation on a real Windows machine | 1 d |
| | **Native Windows total** | **12–15 d** |

**Order: L1 → L2 → L6 → L5 → L3/L4 → L7 → S1 → (W1…W9 only on demand).**

Linux first is confirmed cheaper. It already compiles and passes, and its pty path works. What remains is a credential store and packaging.

Coordination with the multi-agent plan:
- Do W7's Rust fake-`claude` during or before that plan's Phase 1 (the adapter refactor), since both rewrite `tests/support`.
- Agree on the per-target `current.json` before either plan ships self-update.
- Nothing here changes the adapter trait. A future Windows meter slots in as another `WeeklyMeter` implementation.

### Open questions for Josh

1. Buy Azure Artifact Signing ($9.99/mo) for Windows, or ship WSL-only and skip Windows signing for now?
2. Native Windows workspace root: `C:\ProgramData\ClaudeDaycare\<SID>` with a custom DACL, or another location?
3. Move release binaries out of the platform's git (GitHub Releases on the public repo), now that there will be 4–5 per release?
