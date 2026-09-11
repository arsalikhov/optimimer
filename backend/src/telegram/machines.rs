//! Machine control: Wake-on-LAN for machines the owner registered
//! (`config::MACHINES`, added via onboarding or the `add_machine` tool).

use super::*;
use crate::config;

/// Send a magic packet to the named machine (or the only one, if the name is
/// empty) via `etherwake`. No sudo: the systemd unit grants the service
/// `AmbientCapabilities=CAP_NET_RAW`, which children inherit across exec and
/// which coexists with NoNewPrivileges (sudo does not).
pub(super) async fn handle_wake(name: &str) -> Reply {
    let known = config::machines();
    if known.is_empty() {
        return Reply::text("No machines are registered yet. Tell me a name and MAC address (e.g. \"add machine desktop aa:bb:cc:dd:ee:ff\") and I'll remember it.");
    }
    let Some(m) = config::find_machine(name) else {
        let names = known.iter().map(|m| m.name.clone()).collect::<Vec<_>>().join(", ");
        return Reply::text(format!("Which machine? I know: {names}."));
    };
    let out = tokio::process::Command::new("etherwake")
        .args(["-i", &m.iface, &m.mac])
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() => Reply::text(format!(
            "🖥️ Magic packet sent to {} (`{}` via {}). Give it ~30s to boot.",
            m.name, m.mac, m.iface
        )),
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr);
            Reply::text(format!(
                "etherwake failed ({}): {}\nDoes the service have AmbientCapabilities=CAP_NET_RAW?",
                o.status,
                err.trim()
            ))
        }
        Err(e) => Reply::text(format!("Couldn't run etherwake: {e}")),
    }
}
