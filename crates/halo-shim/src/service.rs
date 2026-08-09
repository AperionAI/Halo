//! `halo service install` / `halo service uninstall` -- turn Halo into an
//! always-on background service so it survives logout and reboot.
//!
//! Halo's whole reason to exist is protecting an always-on agent box, but the
//! documented path stopped at `halo serve` in a foreground shell (screen/tmux).
//! Close the terminal and every agent request gets connection-refused. This
//! command generates the pieces a real service install needs -- a wrapper that
//! sources the vault passphrase, a fixed data directory, log files, and (on
//! macOS) a LaunchDaemon -- so an operator doesn't have to hand-build them.
//!
//! macOS (launchd) is automated here because that's the environment reported
//! from the field. Linux (systemd) ships as a documented unit template in
//! `docs/CLAW_BOX_SETUP.md` for now; see the `not(target_os = "macos")` stub.

#[cfg(target_os = "macos")]
pub use macos::{install, uninstall};

#[cfg(not(target_os = "macos"))]
pub use other::{install, uninstall};

#[cfg(target_os = "macos")]
mod macos {
    use anyhow::{anyhow, bail, Context, Result};
    use base64::Engine;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::process::Command;

    const LABEL: &str = "ai.aperion.halo";
    const PLIST: &str = "/Library/LaunchDaemons/ai.aperion.halo.plist";
    const WRAPPER_DIR: &str = "/usr/local/libexec/halo";
    const WRAPPER: &str = "/usr/local/libexec/halo/halo-serve.sh";
    const CONF_DIR: &str = "/usr/local/etc/halo";
    const PASSPHRASE_FILE: &str = "/usr/local/etc/halo/vault-passphrase";
    const LOG_DIR: &str = "/usr/local/var/log/halo";
    const OUT_LOG: &str = "/usr/local/var/log/halo/halo.log";
    const ERR_LOG: &str = "/usr/local/var/log/halo/halo.err.log";
    /// Fixed data dir for the service, independent of any user's `~`. This is
    /// deliberate: relying on `~/.halo` means the store moves with whichever
    /// user launchd resolves, which is exactly the "report shows $0" confusion
    /// from the field. A service install pins one directory for all users.
    const HALO_HOME_DIR: &str = "/usr/local/var/halo";

    pub fn install(user: Option<String>) -> Result<()> {
        require_root("install")?;

        let service_user = resolve_service_user(user);
        let bin = resolve_binary()?;

        // Directories: wrapper, config, logs, data.
        for dir in [WRAPPER_DIR, CONF_DIR, LOG_DIR, HALO_HOME_DIR] {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {dir}"))?;
        }

        // The service user owns the data dir and logs so `halo serve` can write.
        chown_recursive(&service_user, HALO_HOME_DIR)?;
        chown_recursive(&service_user, LOG_DIR)?;

        let generated = ensure_passphrase(&service_user)?;

        write_wrapper(&bin)?;
        write_plist(&service_user, &bin)?;

        bootstrap_daemon()?;

        println!("Installed Halo as a launchd service.");
        println!("  Label:      {LABEL}");
        println!("  Runs as:    {service_user}");
        println!("  Binary:     {}", bin.display());
        println!("  Data dir:   {HALO_HOME_DIR}");
        println!("  Logs:       {OUT_LOG}");
        println!("            {ERR_LOG}");
        println!("  Passphrase: {PASSPHRASE_FILE} (mode 0600, owned by {service_user})");
        if let Some(pass) = generated {
            println!(
                "\n  A NEW vault passphrase was generated. Back it up somewhere safe NOW -- \
                 without it the encrypted provider keys cannot be decrypted:\n\n      {pass}\n"
            );
        } else {
            println!("\n  Reused the existing vault passphrase at {PASSPHRASE_FILE}.");
        }

        println!(
            "\nNext: register your agent AS THE SERVICE USER, with the same passphrase the \
             service uses, so the encrypted key it writes is the one the service can read:\n\n    \
             sudo -u {service_user} \\\n      env HALO_HOME={HALO_HOME_DIR} \
             HALO_VAULT_PASSPHRASE=\"$(sudo cat {PASSPHRASE_FILE})\" \\\n      {} agent add <name> \
             --provider anthropic\n\nThen restart the service so it picks up the new agent:\n\n    \
             sudo launchctl kickstart -k system/{LABEL}\n",
            bin.display()
        );
        Ok(())
    }

    pub fn uninstall() -> Result<()> {
        require_root("uninstall")?;

        // Best-effort teardown of the running daemon; ignore "not loaded".
        let _ = Command::new("launchctl")
            .args(["bootout", &format!("system/{LABEL}")])
            .status();

        for path in [PLIST, WRAPPER] {
            match std::fs::remove_file(path) {
                Ok(()) => println!("Removed {path}"),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(anyhow!("removing {path}: {e}")),
            }
        }

        println!(
            "\nHalo launchd service removed. Left in place on purpose:\n  \
             {PASSPHRASE_FILE}  (deleting it makes the encrypted keys unrecoverable)\n  \
             {HALO_HOME_DIR}    (spend ledger, cache, audit log)\n\
             Delete those manually if you want a clean slate."
        );
        Ok(())
    }

    fn require_root(action: &str) -> Result<()> {
        if effective_uid() == Some(0) {
            return Ok(());
        }
        bail!(
            "`halo service {action}` writes to {PLIST} and {WRAPPER_DIR}, so it must run as \
             root. Re-run with sudo, e.g.:\n\n    sudo {} service {action}\n",
            std::env::current_exe()
                .ok()
                .and_then(|p| p.to_str().map(str::to_string))
                .unwrap_or_else(|| "halo".to_string())
        );
    }

    /// Which user the daemon runs as. Prefer an explicit `--user`, then the
    /// user who invoked sudo (`$SUDO_USER`), then `root`.
    fn resolve_service_user(explicit: Option<String>) -> String {
        explicit
            .filter(|s| !s.is_empty())
            .or_else(|| std::env::var("SUDO_USER").ok().filter(|s| !s.is_empty() && s != "root"))
            .unwrap_or_else(|| "root".to_string())
    }

    /// Prefer a shared, on-PATH binary the service user can also reach over
    /// whatever path this process happens to have been launched from (which
    /// may be a per-user `~/.local/bin` the service user can't read).
    fn resolve_binary() -> Result<std::path::PathBuf> {
        let shared = Path::new("/usr/local/bin/halo");
        if shared.exists() {
            return Ok(shared.to_path_buf());
        }
        let exe = std::env::current_exe().context("resolving the halo binary path")?;
        eprintln!(
            "note: /usr/local/bin/halo not found; the service will run {} instead. If the \
             service user can't read that path, copy the binary to /usr/local/bin first \
             (or re-install via HALO_INSTALL_DIR=/usr/local/bin).",
            exe.display()
        );
        Ok(exe)
    }

    /// Create the passphrase file if it doesn't already exist. Returns the
    /// freshly generated passphrase (to print once for backup) or `None` if an
    /// existing one was reused.
    fn ensure_passphrase(service_user: &str) -> Result<Option<String>> {
        if Path::new(PASSPHRASE_FILE).exists() {
            chown(service_user, PASSPHRASE_FILE)?;
            set_mode(PASSPHRASE_FILE, 0o600)?;
            return Ok(None);
        }
        let mut buf = [0u8; 32];
        getrandom::getrandom(&mut buf).map_err(|e| anyhow!("rng: {e}"))?;
        let pass = base64::engine::general_purpose::STANDARD.encode(buf);
        std::fs::write(PASSPHRASE_FILE, &pass).with_context(|| format!("writing {PASSPHRASE_FILE}"))?;
        set_mode(PASSPHRASE_FILE, 0o600)?;
        chown(service_user, PASSPHRASE_FILE)?;
        Ok(Some(pass))
    }

    fn write_wrapper(bin: &Path) -> Result<()> {
        let script = format!(
            "#!/bin/sh\n\
             # Generated by `halo service install`. Sources the vault passphrase\n\
             # (the OS keychain is unreachable outside a GUI session) and pins a\n\
             # fixed data directory, then launches the proxy.\n\
             set -eu\n\
             export HALO_HOME=\"{HALO_HOME_DIR}\"\n\
             export HALO_VAULT_PASSPHRASE=\"$(cat {PASSPHRASE_FILE})\"\n\
             exec \"{}\" serve\n",
            bin.display()
        );
        std::fs::write(WRAPPER, script).with_context(|| format!("writing {WRAPPER}"))?;
        set_mode(WRAPPER, 0o755)?;
        Ok(())
    }

    fn write_plist(service_user: &str, _bin: &Path) -> Result<()> {
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{WRAPPER}</string>
    </array>
    <key>UserName</key>
    <string>{service_user}</string>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>WorkingDirectory</key>
    <string>{HALO_HOME_DIR}</string>
    <key>StandardOutPath</key>
    <string>{OUT_LOG}</string>
    <key>StandardErrorPath</key>
    <string>{ERR_LOG}</string>
</dict>
</plist>
"#
        );
        std::fs::write(PLIST, plist).with_context(|| format!("writing {PLIST}"))?;
        set_mode(PLIST, 0o644)?;
        Ok(())
    }

    fn bootstrap_daemon() -> Result<()> {
        // If a previous copy is loaded, bootout first so bootstrap succeeds.
        let _ = Command::new("launchctl")
            .args(["bootout", &format!("system/{LABEL}")])
            .status();
        let status = Command::new("launchctl")
            .args(["bootstrap", "system", PLIST])
            .status()
            .context("running launchctl bootstrap")?;
        if !status.success() {
            bail!(
                "launchctl bootstrap system {PLIST} failed. The files are in place; you can \
                 retry with `sudo launchctl bootstrap system {PLIST}`."
            );
        }
        let _ = Command::new("launchctl")
            .args(["enable", &format!("system/{LABEL}")])
            .status();
        Ok(())
    }

    fn effective_uid() -> Option<u32> {
        let out = Command::new("id").arg("-u").output().ok()?;
        String::from_utf8(out.stdout).ok()?.trim().parse().ok()
    }

    fn set_mode(path: &str, mode: u32) -> Result<()> {
        let mut perms = std::fs::metadata(path)
            .with_context(|| format!("stat {path}"))?
            .permissions();
        perms.set_mode(mode);
        std::fs::set_permissions(path, perms).with_context(|| format!("chmod {path}"))?;
        Ok(())
    }

    fn chown(user: &str, path: &str) -> Result<()> {
        run_chown(&[user, path])
    }

    fn chown_recursive(user: &str, path: &str) -> Result<()> {
        run_chown(&["-R", user, path])
    }

    fn run_chown(args: &[&str]) -> Result<()> {
        let status = Command::new("chown")
            .args(args)
            .status()
            .context("running chown")?;
        if !status.success() {
            bail!("chown {} failed", args.join(" "));
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn explicit_user_wins_over_env() {
            // An explicit --user is honored regardless of $SUDO_USER.
            assert_eq!(resolve_service_user(Some("openclaw".into())), "openclaw");
        }

        #[test]
        fn empty_explicit_user_is_ignored() {
            // An empty string must not be treated as a real user.
            let u = resolve_service_user(Some(String::new()));
            assert!(u == "root" || !u.is_empty());
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod other {
    use anyhow::{bail, Result};

    pub fn install(_user: Option<String>) -> Result<()> {
        bail!(
            "`halo service install` automates macOS (launchd) only for now. On Linux, use the \
             systemd unit template in docs/CLAW_BOX_SETUP.md; on Windows, run `halo serve` under \
             a service manager of your choice."
        )
    }

    pub fn uninstall() -> Result<()> {
        bail!(
            "`halo service uninstall` automates macOS (launchd) only for now. On Linux, disable \
             and remove the systemd unit you created from docs/CLAW_BOX_SETUP.md."
        )
    }
}
