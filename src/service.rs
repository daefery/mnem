//! The background `rvn watch` service: a systemd user unit on Linux (and WSL), a
//! launchd agent on macOS. One place decides which, where its file lives, and how to
//! start, stop and ask about it, so install, uninstall, the health checks and the
//! viewer's restart button agree on every system.

use crate::db;
use std::path::PathBuf;
use std::process::Command;

/// The launchd label and systemd unit name.
pub const LABEL: &str = "dev.ravnori.watch";
pub const UNIT: &str = "ravnori-watch.service";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Manager {
    Systemd,
    Launchd,
}

/// The service manager of this system.
pub fn manager() -> Manager {
    if cfg!(target_os = "macos") {
        Manager::Launchd
    } else {
        Manager::Systemd
    }
}

impl Manager {
    /// Where the service definition is written.
    pub fn file(self) -> PathBuf {
        match self {
            Manager::Systemd => db::home().join(".config/systemd/user").join(UNIT),
            Manager::Launchd => db::home()
                .join("Library/LaunchAgents")
                .join(format!("{LABEL}.plist")),
        }
    }

    /// The service definition running `<bin> watch`, restarted whenever it exits.
    pub fn definition(self, bin: &str) -> String {
        match self {
            Manager::Systemd => format!(
                "[Unit]\nDescription=ravnori transcript watcher\n\n[Service]\nExecStart={bin} watch\nRestart=always\nRestartSec=10\nNice=10\n\n[Install]\nWantedBy=default.target\n"
            ),
            // KeepAlive restarts it after any exit (the viewer's restart button relies on
            // that); ThrottleInterval spaces restarts like RestartSec. launchd starts
            // agents with a bare PATH, so the usual install folders are added.
            Manager::Launchd => {
                let log = db::data_dir().join("watch.log");
                format!(
                    r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array><string>{}</string><string>watch</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>ProcessType</key><string>Background</string>
  <key>Nice</key><integer>10</integer>
  <key>EnvironmentVariables</key>
  <dict><key>PATH</key><string>{}/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string></dict>
  <key>StandardOutPath</key><string>{}</string>
  <key>StandardErrorPath</key><string>{}</string>
</dict>
</plist>
"#,
                    xml(bin),
                    xml(&db::home().to_string_lossy()),
                    xml(&log.to_string_lossy()),
                    xml(&log.to_string_lossy())
                )
            }
        }
    }

    /// Load and start the service written to `file()`; whether that worked.
    pub fn start(self) -> bool {
        match self {
            Manager::Systemd => {
                run("systemctl", &["--user", "daemon-reload"])
                    && run("systemctl", &["--user", "enable", "--now", UNIT])
            }
            Manager::Launchd => {
                let target = gui_target();
                let file = self.file();
                // Replace a loaded copy so a changed definition takes effect.
                let _ = run("launchctl", &["bootout", &format!("{target}/{LABEL}")]);
                run(
                    "launchctl",
                    &["bootstrap", &target, &file.to_string_lossy()],
                )
            }
        }
    }

    /// Stop the service and unload it (its file is removed by the caller).
    pub fn stop(self) {
        match self {
            Manager::Systemd => {
                let _ = run("systemctl", &["--user", "disable", "--now", UNIT]);
            }
            Manager::Launchd => {
                let _ = run(
                    "launchctl",
                    &["bootout", &format!("{}/{LABEL}", gui_target())],
                );
            }
        }
    }

    /// After the file is removed (systemd keeps a cached copy until reloaded).
    pub fn forget(self) {
        if self == Manager::Systemd {
            let _ = run("systemctl", &["--user", "daemon-reload"]);
        }
    }

    /// Whether the service is running now.
    pub fn active(self) -> bool {
        match self {
            Manager::Systemd => run("systemctl", &["--user", "is-active", "--quiet", UNIT]),
            Manager::Launchd => Command::new("launchctl")
                .args(["print", &format!("{}/{LABEL}", gui_target())])
                .output()
                .is_ok_and(|o| {
                    o.status.success()
                        && String::from_utf8_lossy(&o.stdout).contains("state = running")
                }),
        }
    }

    /// Whether this system can run the service at all (no systemd: not without help).
    pub fn can_start(self) -> bool {
        self != Manager::Systemd || systemd_running()
    }

    /// The command a user runs to start it by hand, or what to do where it cannot run.
    pub fn start_hint(self) -> String {
        match self {
            Manager::Systemd if !systemd_running() => no_systemd_hint(),
            Manager::Systemd => format!("systemctl --user enable --now {UNIT}"),
            Manager::Launchd => {
                format!("launchctl bootstrap gui/$(id -u) {}", self.file().display())
            }
        }
    }

    /// The command a user runs to see how it is doing.
    pub fn status_hint(self) -> String {
        match self {
            Manager::Systemd => format!("systemctl --user status {UNIT}"),
            Manager::Launchd => format!("launchctl print gui/$(id -u)/{LABEL}"),
        }
    }
}

/// Whether systemd runs this system (it does not on WSL unless turned on, nor in most
/// containers): the test systemd itself documents, sd_booted().
fn systemd_running() -> bool {
    std::path::Path::new("/run/systemd/system").is_dir()
}

/// What to do where there is no systemd: WSL can turn it on; anywhere, `rvn watch`
/// can run in a terminal. Without one of them no memories are made.
fn no_systemd_hint() -> String {
    // /run/WSL exists inside a WSL distro, not in a container on a WSL kernel (whose
    // kernel name says "microsoft" too), and unlike WSL_DISTRO_NAME it reaches services.
    let wsl = std::path::Path::new("/run/WSL").is_dir();
    if wsl {
        format!(
            "this WSL has no systemd: add `[boot]` and `systemd=true` to /etc/wsl.conf, run `wsl --shutdown` in Windows, then `systemctl --user enable --now {UNIT}`; or keep `rvn watch` running in a terminal"
        )
    } else {
        "this system has no systemd: keep `rvn watch` running (a terminal, tmux, or your init system)".to_string()
    }
}

/// Whether this process is the service, run by a manager that restarts it after it
/// exits (the viewer's restart button exits and lets the manager start it again).
pub fn supervised_with_restart() -> bool {
    match manager() {
        // systemd sets INVOCATION_ID; the unit's Restart= policy must bring it back.
        Manager::Systemd => {
            std::env::var_os("INVOCATION_ID").is_some()
                && systemd_restart_policy().is_some_and(|p| p == "always" || p == "on-failure")
        }
        // launchd sets XPC_SERVICE_NAME to the job's label; ravnori's agent has KeepAlive.
        Manager::Launchd => std::env::var("XPC_SERVICE_NAME").is_ok_and(|n| n == LABEL),
    }
}

/// The Restart= policy of the systemd unit running this process, read from its cgroup.
fn systemd_restart_policy() -> Option<String> {
    let cgroup = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let unit = cgroup
        .lines()
        .flat_map(|l| l.rsplit('/'))
        .find(|p| p.ends_with(".service"))?
        .to_string();
    let mut cmd = Command::new("systemctl");
    if cgroup.contains("/user@") {
        cmd.arg("--user");
    }
    let out = cmd
        .args(["show", "-p", "Restart", "--value", &unit])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// launchd's domain for this user's agents.
fn gui_target() -> String {
    #[cfg(unix)]
    let uid = unsafe { libc::getuid() };
    #[cfg(not(unix))]
    let uid = 0;
    format!("gui/{uid}")
}

fn run(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd)
        .args(args)
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Escape text for a plist string.
fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_manager_runs_ravnori_watch_and_restarts_it() {
        let unit = Manager::Systemd.definition("/opt/rvn");
        assert!(unit.contains("ExecStart=/opt/rvn watch") && unit.contains("Restart=always"));
        let plist = Manager::Launchd.definition("/Users/me/my <tools>/rvn");
        assert!(
            plist.contains("<string>/Users/me/my &lt;tools&gt;/rvn</string><string>watch</string>")
        );
        assert!(plist.contains("<key>KeepAlive</key><true/>"));
        assert!(plist.contains(&format!("<string>{LABEL}</string>")));
        assert!(
            plist.contains("/opt/homebrew/bin"),
            "launchd's bare PATH is extended"
        );
        assert!(
            Manager::Launchd
                .file()
                .ends_with("Library/LaunchAgents/dev.ravnori.watch.plist")
        );
        assert!(
            Manager::Systemd
                .file()
                .ends_with(".config/systemd/user/ravnori-watch.service")
        );
    }
}
