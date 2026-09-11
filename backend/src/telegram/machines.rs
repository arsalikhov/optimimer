//! Machine control (`/wake_mercury`): Wake-on-LAN for the HPC box.

use super::*;

/// `/wake_mercury` — Wake-on-LAN via `etherwake` (raw ethernet frame on
/// WOL_IFACE, default eth0). No sudo: the systemd unit grants the service
/// `AmbientCapabilities=CAP_NET_RAW`, which children inherit across exec and
/// which coexists with NoNewPrivileges (sudo does not).
pub(super) async fn handle_wake_mercury() -> Reply {
    let mac = std::env::var("MERCURY_MAC").unwrap_or_else(|_| "00:00:00:00:00:00".to_string());
    let iface = std::env::var("WOL_IFACE").unwrap_or_else(|_| "eth0".to_string());
    let out = tokio::process::Command::new("etherwake")
        .args(["-i", &iface, &mac])
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() => Reply::text(format!(
            "🖥️ Magic packet sent to mercury (`{mac}` via {iface}). Give it ~30s to boot."
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
