//! Machines this window can drive: This PC, or another computer's Zeron
//! engine reached over SSH. That engine runs as the other machine's daemon
//! (`zeron daemon install`) and stays loopback-only there; Zeron forwards a
//! free local port to it with `ssh -N -L` and attaches to that port, never
//! embedding an engine of its own. Chats, projects and Pi live on the
//! machine; this window only views and drives them.

use tokio::io::AsyncReadExt;

use crate::state::EngineHandle;

/// The port a machine's daemon serves on its own loopback.
pub const REMOTE_ENGINE_PORT: u16 = 27654;
/// Half-second probes an opening tunnel gets before the switch gives up.
const OPEN_TRIES: usize = 40;

/// Why `target` is not an SSH destination Zeron passes on: `user@host` or
/// an `~/.ssh/config` alias, never an option.
pub fn validate_target(target: &str) -> Result<(), &'static str> {
    let target = target.trim();
    if target.is_empty() {
        return Err("Enter user@host");
    }
    if target.starts_with('-') {
        return Err("A machine can't start with \"-\"");
    }
    if target.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("A machine has no spaces");
    }
    Ok(())
}

/// What the switcher calls `target` without a name: its host.
pub fn default_name(target: &str) -> String {
    target
        .rsplit_once('@')
        .map_or(target, |(_, host)| host)
        .to_owned()
}

/// `ssh`'s arguments for the tunnel. No prompts (a key or an agent must
/// answer; a new host key is trusted on first use), fail if the port can't
/// be forwarded, and notice a dead link within a minute and a half.
pub fn tunnel_args(target: &str, local_port: u16) -> Vec<String> {
    let mut args: Vec<String> = [
        "-N",
        "-o",
        "BatchMode=yes",
        "-o",
        "StrictHostKeyChecking=accept-new",
        "-o",
        "ExitOnForwardFailure=yes",
        "-o",
        "ConnectTimeout=10",
        "-o",
        "ServerAliveInterval=30",
        "-o",
        "ServerAliveCountMax=3",
        "-L",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    args.push(format!("{local_port}:127.0.0.1:{REMOTE_ENGINE_PORT}"));
    args.push("--".into());
    args.push(target.trim().to_owned());
    args
}

/// What the window says when the tunnel to `target` ended: ssh's own last
/// words, or why it likely failed.
pub fn tunnel_failure(target: &str, stderr: &str) -> String {
    let reason = stderr
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .unwrap_or("ssh exited");
    format!("Couldn't reach {target} over SSH: {reason}")
}

/// The open `ssh -N -L` to a machine; dropping it closes the tunnel.
pub struct Tunnel {
    _child: zeron_harness::process::Child,
    pub port: u16,
}

/// A free loopback port to forward.
fn free_port() -> std::io::Result<u16> {
    Ok(std::net::TcpListener::bind(("127.0.0.1", 0))?
        .local_addr()?
        .port())
}

/// Tunnel to `target` and attach to its engine. Runs on Tokio.
pub async fn open(target: String) -> Result<(Tunnel, EngineHandle), String> {
    use zeron_harness::process::{Command, Stdio};
    let port = free_port().map_err(|err| format!("No free local port: {err}"))?;
    let mut command = Command::new("ssh");
    command
        .args(tunnel_args(&target, port))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|err| format!("Couldn't start ssh: {err}"))?;
    for _ in 0..OPEN_TRIES {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        if let Ok(Some(_)) = child.try_wait() {
            let mut stderr = String::new();
            if let Some(mut pipe) = child.stderr.take() {
                let _ = pipe.read_to_string(&mut stderr).await;
            }
            return Err(tunnel_failure(&target, &stderr));
        }
        if let Some(handle) = EngineHandle::attach(port).await {
            return Ok((
                Tunnel {
                    _child: child,
                    port,
                },
                handle,
            ));
        }
    }
    Err(format!(
        "No Zeron engine answers on {target}. Run `zeron daemon install` there."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_machine_is_an_ssh_destination_never_an_option() {
        assert!(validate_target("akshat@lab-box").is_ok());
        assert!(validate_target(" lab ").is_ok());
        assert_eq!(validate_target("  "), Err("Enter user@host"));
        assert!(validate_target("-oProxyCommand=evil").is_err());
        assert!(validate_target("user@host extra").is_err());
    }

    #[test]
    fn a_machine_without_a_name_is_called_by_its_host() {
        assert_eq!(default_name("akshat@lab-box"), "lab-box");
        assert_eq!(default_name("lab"), "lab");
    }

    #[test]
    fn the_tunnel_forwards_a_local_port_to_the_machines_engine() {
        let args = tunnel_args(" akshat@lab-box ", 41000);
        assert_eq!(args.first().map(String::as_str), Some("-N"));
        assert!(args.iter().any(|a| a == "BatchMode=yes"));
        assert!(args.iter().any(|a| a == "ExitOnForwardFailure=yes"));
        let forward = args.iter().position(|a| a == "-L").unwrap();
        assert_eq!(args[forward + 1], "41000:127.0.0.1:27654");
        // The destination comes last, after `--`: it is never read as an option.
        assert_eq!(&args[args.len() - 2..], ["--", "akshat@lab-box"]);
    }

    #[test]
    fn a_failed_tunnel_says_what_ssh_said() {
        assert_eq!(
            tunnel_failure("lab", "Warning: x\nPermission denied (publickey).\n"),
            "Couldn't reach lab over SSH: Permission denied (publickey)."
        );
        assert_eq!(
            tunnel_failure("lab", ""),
            "Couldn't reach lab over SSH: ssh exited"
        );
    }
}
