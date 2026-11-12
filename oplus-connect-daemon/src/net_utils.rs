use std::path::PathBuf;
use std::process::Command;

/// Per-user configuration directory shared by the daemon and GUI.
pub fn config_dir() -> PathBuf {
    std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            PathBuf::from(home).join(".config")
        })
}

/// Active Wi-Fi SSID of the machine, if any.
pub fn current_ssid() -> Option<String> {
    let out = Command::new("nmcli")
        .args(["-t", "-f", "active,ssid", "dev", "wifi"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        if let Some(ssid) = line.strip_prefix("yes:") {
            let ssid = ssid.trim();
            if !ssid.is_empty() {
                return Some(ssid.to_string());
            }
        }
    }
    None
}

/// Local hostname used as the PC display name in the pairing payload.
pub fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "Linux PC".to_string())
}

/// Protocol device id used in SDP (`PDID`) and in the pairing QR (`dvd`).
/// The phone prints/stores it as 12 uppercase hex characters.
pub fn pdid() -> String {
    std::env::var("OPLUS_PDID")
        .unwrap_or_else(|_| local_device_id().trim_start_matches("pc_").to_string())
        .to_uppercase()
}

/// Stable-ish local device id derived from the machine-id.
pub fn local_device_id() -> String {
    let raw = std::fs::read_to_string("/etc/machine-id")
        .or_else(|_| std::fs::read_to_string("/var/lib/dbus/machine-id"))
        .unwrap_or_default();
    let hex: String = raw
        .trim()
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .collect();
    if hex.len() >= 12 {
        format!("pc_{}", &hex[..12])
    } else {
        "pc_linux_000000000000".to_string()
    }
}

use std::net::Ipv4Addr;

/// Best-effort discovery of the local IPv4 address inside the same subnet as `peer_ip`.
pub fn local_ip_for(peer_ip: Option<&str>) -> Option<String> {
    let output = Command::new("ip")
        .args(["-4", "-o", "addr", "show"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);

    let mut candidates: Vec<(Ipv4Addr, u8, String)> = Vec::new();
    for line in text.lines() {
        // 2: enp14s0    inet 192.168.1.5/24 brd 192.168.1.255 scope global dynamic enp14s0
        let mut it = line.split_whitespace();
        let _idx = it.next();
        let _ifname = it.next();
        if it.next() != Some("inet") {
            continue;
        }
        let Some(cidr) = it.next() else { continue };
        let Some((addr_s, prefix_s)) = cidr.split_once('/') else {
            continue;
        };
        let (Ok(addr), Ok(prefix)) = (addr_s.parse::<Ipv4Addr>(), prefix_s.parse::<u8>()) else {
            continue;
        };
        if addr.is_loopback() {
            continue;
        }
        candidates.push((addr, prefix, addr_s.to_string()));
    }

    if let Some(peer) = peer_ip.and_then(|ip| ip.parse::<Ipv4Addr>().ok()) {
        let peer_bits = u32::from(peer);
        for (addr, prefix, s) in &candidates {
            if *prefix == 0 || *prefix > 32 {
                continue;
            }
            let mask = if *prefix == 32 {
                u32::MAX
            } else {
                !((1u32 << (32 - *prefix)) - 1)
            };
            if (u32::from(*addr) & mask) == (peer_bits & mask) {
                return Some(s.clone());
            }
        }
    }

    candidates.first().map(|(_, _, s)| s.clone())
}

/// All private IPv4 addresses of local network interfaces.
pub fn private_interface_ips() -> Vec<Ipv4Addr> {
    #[cfg(not(windows))]
    {
        let Ok(output) = Command::new("ip")
            .args(["-4", "-o", "addr", "show"])
            .output()
        else {
            return Vec::new();
        };
        let text = String::from_utf8_lossy(&output.stdout);
        let mut ips = Vec::new();
        for line in text.lines() {
            let mut fields = line.split_whitespace();
            let _ = fields.next();
            let _ = fields.next();
            if fields.next() != Some("inet") {
                continue;
            }
            let Some(address) = fields.next().and_then(|cidr| cidr.split('/').next()) else {
                continue;
            };
            if let Ok(ip) = address.parse::<Ipv4Addr>()
                && ip.is_private()
                && !ips.contains(&ip)
            {
                ips.push(ip);
            }
        }
        ips
    }
    #[cfg(windows)]
    {
        Vec::new()
    }
}

/// Most recent non-local IPv4 address that sent an SDP query.
///
/// Tunnelled or relayed sessions arrive with this machine's own address as
/// their peer; the SDP source is then the phone's real LAN address.
static LAST_LAN_PEER: std::sync::Mutex<Option<(Ipv4Addr, std::time::Instant)>> =
    std::sync::Mutex::new(None);

/// Records the source of an SDP query, ignoring loopback and local addresses.
pub fn remember_lan_peer(ip: Ipv4Addr) {
    if ip.is_loopback() || private_interface_ips().contains(&ip) {
        return;
    }
    if let Ok(mut slot) = LAST_LAN_PEER.lock() {
        *slot = Some((ip, std::time::Instant::now()));
    }
}

/// Last remembered LAN peer, if it was seen within `max_age`.
pub fn last_lan_peer(max_age: std::time::Duration) -> Option<Ipv4Addr> {
    let slot = LAST_LAN_PEER.lock().ok()?;
    let (ip, seen) = (*slot)?;
    (seen.elapsed() <= max_age).then_some(ip)
}

/// IPv4 broadcast addresses for private-network interfaces.
///
/// Sending only to `255.255.255.250` can select a VPN route instead of the LAN
/// interface, so discovery also uses each local private subnet's directed
/// broadcast address.
pub fn private_broadcast_ips() -> Vec<Ipv4Addr> {
    #[cfg(windows)]
    {
        return windows_private_broadcast_ips();
    }

    #[cfg(not(windows))]
    {
        private_broadcast_ips_from_ip_command()
    }
}

#[cfg(not(windows))]
fn private_broadcast_ips_from_ip_command() -> Vec<Ipv4Addr> {
    let Ok(output) = Command::new("ip")
        .args(["-4", "-o", "addr", "show"])
        .output()
    else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&output.stdout);
    parse_private_broadcast_ips(&text)
}

#[cfg(windows)]
fn windows_private_broadcast_ips() -> Vec<Ipv4Addr> {
    // `ip -4 -o addr show` is unavailable on a normal Windows installation.
    // Query the same address/prefix data through the built-in NetTCPIP cmdlet.
    let output = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Get-NetIPAddress -AddressFamily IPv4 | ForEach-Object { \"$($_.IPAddress)/$($_.PrefixLength)\" }",
        ])
        .output()
        .ok();
    let Some(output) = output else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&output.stdout);
    parse_cidr_broadcast_ips(&text)
}

#[cfg(any(not(windows), test))]
fn parse_private_broadcast_ips(text: &str) -> Vec<Ipv4Addr> {
    let mut broadcasts = Vec::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let _ = fields.next();
        let _ = fields.next();
        if fields.next() != Some("inet") {
            continue;
        }
        let Some(address) = fields.next().and_then(|cidr| cidr.split('/').next()) else {
            continue;
        };
        let Ok(address) = address.parse::<Ipv4Addr>() else {
            continue;
        };
        if !address.is_private() {
            continue;
        }
        while let Some(field) = fields.next() {
            if field == "brd" {
                if let Some(broadcast) = fields.next().and_then(|s| s.parse::<Ipv4Addr>().ok())
                    && !broadcasts.contains(&broadcast)
                {
                    broadcasts.push(broadcast);
                }
                break;
            }
        }
    }
    broadcasts
}

#[cfg(any(windows, test))]
fn parse_cidr_broadcast_ips(text: &str) -> Vec<Ipv4Addr> {
    let mut broadcasts = Vec::new();
    for line in text.lines() {
        let Some((address, prefix)) = line.trim().split_once('/') else {
            continue;
        };
        let (Ok(address), Ok(prefix)) = (address.parse::<Ipv4Addr>(), prefix.parse::<u8>()) else {
            continue;
        };
        if !address.is_private() || prefix > 32 {
            continue;
        }
        let mask = if prefix == 0 {
            0
        } else {
            u32::MAX << (32 - prefix)
        };
        let broadcast = Ipv4Addr::from(u32::from(address) | !mask);
        if !broadcasts.contains(&broadcast) {
            broadcasts.push(broadcast);
        }
    }
    broadcasts
}

#[cfg(test)]
mod tests {
    use super::{parse_cidr_broadcast_ips, parse_private_broadcast_ips};
    use std::net::Ipv4Addr;

    #[test]
    fn selects_private_subnet_broadcasts_and_ignores_virtual_benchmark_network() {
        let input = concat!(
            "2: enp14s0 inet 192.168.1.5/24 brd 192.168.1.255 scope global dynamic enp14s0\n",
            "3: wlp15s0 inet 192.168.2.207/24 brd 192.168.2.255 scope global dynamic wlp15s0\n",
            "4: Sparkle inet 198.18.0.1/30 brd 198.18.0.3 scope global Sparkle\n",
            "5: lo inet 127.0.0.1/8 scope host lo\n",
        );

        assert_eq!(
            parse_private_broadcast_ips(input),
            vec![
                Ipv4Addr::new(192, 168, 1, 255),
                Ipv4Addr::new(192, 168, 2, 255),
            ]
        );
    }

    #[test]
    fn computes_broadcasts_from_windows_cidr_output() {
        assert_eq!(
            parse_cidr_broadcast_ips("192.168.2.233/24\n192.168.1.5/24\n127.0.0.1/8\n"),
            vec![
                Ipv4Addr::new(192, 168, 2, 255),
                Ipv4Addr::new(192, 168, 1, 255),
            ]
        );
    }
}
