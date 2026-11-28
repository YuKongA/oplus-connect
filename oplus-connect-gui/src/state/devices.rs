use super::{AppState, DeviceInfo, RemoteDevicePayload};
use std::net::Ipv4Addr;

/// Collected once per batch on a background worker. Device state and its reducer
/// must never shell out to `ip` or read the machine-id on the foreground thread.
pub(crate) struct LocalDeviceInfo {
    pdid: String,
    addresses: Vec<Ipv4Addr>,
}

impl LocalDeviceInfo {
    pub(crate) fn discover() -> Self {
        use oplus_connect_daemon::net_utils::{pdid, private_interface_ips};
        Self {
            pdid: pdid(),
            addresses: private_interface_ips(),
        }
    }

    fn is_peer(&self, id: &str) -> bool {
        !id.is_empty() && !id.eq_ignore_ascii_case(&self.pdid)
    }

    fn is_local_address(&self, address: &str) -> bool {
        address
            .trim()
            .parse::<Ipv4Addr>()
            .is_ok_and(|ip| ip.is_loopback() || self.addresses.contains(&ip))
    }

    pub(crate) fn sanitize_device(&self, device: &mut DeviceInfo) -> bool {
        if device
            .ip_address
            .as_deref()
            .is_some_and(|address| address.trim().is_empty() || self.is_local_address(address))
        {
            device.ip_address = None;
        }
        self.is_peer(&device.device_id)
    }

    pub(crate) fn sanitize_remote(&self, remote: &mut Option<RemoteDevicePayload>) {
        if let Some(payload) = remote {
            if !self.is_peer(&payload.device_id) {
                *remote = None;
            } else if self.is_local_address(&payload.device_address) {
                payload.device_address.clear();
            }
        }
    }
}

impl DeviceInfo {
    fn from_remote(remote: &RemoteDevicePayload) -> Self {
        Self {
            device_id: remote.device_id.clone(),
            device_name: remote.device_name.clone(),
            device_model: remote.device_model.clone(),
            device_type: remote.device_type.clone(),
            brand: remote.brand.clone(),
            os_version: remote.os_version.clone(),
            battery: remote.battery_level,
            is_charging: remote.battery_charging,
            connect_type: remote.connect_type.clone(),
            ip_address: (!remote.device_address.trim().is_empty())
                .then(|| remote.device_address.clone()),
            is_connected: true,
            last_seen: 0,
        }
    }

    fn same_cache_fields(&self, other: &Self) -> bool {
        self.device_id == other.device_id
            && self.device_name == other.device_name
            && self.device_model == other.device_model
            && self.device_type == other.device_type
            && self.brand == other.brand
            && self.os_version == other.os_version
            && self.connect_type == other.connect_type
            && self.ip_address == other.ip_address
            && self.is_connected == other.is_connected
    }

    fn same_battery(&self, other: &Self) -> bool {
        self.battery == other.battery && self.is_charging == other.is_charging
    }

    fn disconnect(&mut self) {
        self.is_connected = false;
        self.battery = 0;
        self.is_charging = false;
    }
}

impl AppState {
    /// `/devices` is sampled before `/get_remote_device`. Fold the newer
    /// observation into the list before applying either to the cache. A failed
    /// list tells us nothing about the other peers' connection state.
    pub(crate) fn reconcile_devices(
        &mut self,
        devices: Option<Vec<DeviceInfo>>,
        remote: Result<Option<RemoteDevicePayload>, String>,
    ) -> bool {
        let complete = devices.is_some();
        let mut live = devices.unwrap_or_default();
        live.retain(|d| d.is_connected);
        // Choose the final active peer once. Only its newer fields should
        // reach the cache, even when the earlier list contained this peer.
        let candidate = if let Ok(Some(r)) = &remote {
            let incoming = DeviceInfo::from_remote(r);
            let index = live
                .iter()
                .position(|d| d.device_id.eq_ignore_ascii_case(&incoming.device_id));
            if let Some(index) = index {
                live[index] = incoming;
                Some(&live[index])
            } else {
                live.push(incoming);
                live.last()
            }
        } else {
            live.first()
        };
        let mut changed = false;
        if complete || candidate.is_some() {
            match (&self.connected_device, candidate) {
                (Some(old), Some(new)) if old.same_cache_fields(new) && old.same_battery(new) => {}
                (None, None) => {}
                _ => {
                    self.connected_device = candidate.cloned();
                    changed = true;
                }
            }
        }
        match remote {
            Ok(details) if self.remote_details != details => {
                self.remote_details = details;
                changed = true;
            }
            // Don't leave the previous peer's details attached to its replacement.
            Err(_)
                if complete
                    && self.remote_details.as_ref().is_some_and(|r| {
                        self.connected_device
                            .as_ref()
                            .is_none_or(|d| !d.device_id.eq_ignore_ascii_case(&r.device_id))
                    }) =>
            {
                self.remote_details = None;
                changed = true;
            }
            _ => {}
        }
        changed |= self.apply_device_list(live, complete);
        changed
    }

    fn apply_device_list(&mut self, live: Vec<DeviceInfo>, complete: bool) -> bool {
        let mut changed = false;
        if complete {
            for known in &mut self.known_devices {
                if known.is_connected
                    && !live
                        .iter()
                        .any(|d| d.device_id.eq_ignore_ascii_case(&known.device_id))
                {
                    known.disconnect();
                    self.known_devices_dirty = true;
                    changed = true;
                }
            }
        }
        let seen = chrono::Utc::now().timestamp();
        for device in live {
            changed |= self.upsert_device(device, seen);
        }
        if changed {
            self.sort_known_devices();
        }
        changed
    }

    /// The producer has checked that this is a connected peer.
    pub(crate) fn remember_connected_device(&mut self, device: DeviceInfo) -> bool {
        let changed = self.upsert_device(device, chrono::Utc::now().timestamp());
        if changed {
            self.sort_known_devices();
        }
        changed
    }

    fn upsert_device(&mut self, mut incoming: DeviceInfo, seen: i64) -> bool {
        if let Some(known) = self
            .known_devices
            .iter_mut()
            .find(|k| k.device_id.eq_ignore_ascii_case(&incoming.device_id))
        {
            if known.same_cache_fields(&incoming) {
                let changed = !known.same_battery(&incoming);
                known.battery = incoming.battery;
                known.is_charging = incoming.is_charging;
                known.last_seen = seen;
                return changed;
            }
            incoming.last_seen = seen;
            *known = incoming;
        } else {
            incoming.last_seen = seen;
            self.known_devices.push(incoming);
        }
        self.known_devices_dirty = true;
        true
    }

    pub(crate) fn mark_device_disconnected(&mut self, device_id: Option<&str>) -> bool {
        let mut changed = false;
        for known in &mut self.known_devices {
            if known.is_connected
                && device_id.is_none_or(|id| known.device_id.eq_ignore_ascii_case(id))
            {
                known.disconnect();
                changed = true;
            }
        }
        if changed {
            self.known_devices_dirty = true;
            self.sort_known_devices();
        }
        changed
    }

    fn sort_known_devices(&mut self) {
        self.known_devices.sort_by(|left, right| {
            right
                .is_connected
                .cmp(&left.is_connected)
                .then_with(|| right.last_seen.cmp(&left.last_seen))
                .then_with(|| left.device_id.cmp(&right.device_id))
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(id: &str) -> DeviceInfo {
        DeviceInfo {
            device_id: id.into(),
            device_type: "phone".into(),
            is_connected: true,
            ..Default::default()
        }
    }

    fn remote(id: &str) -> RemoteDevicePayload {
        RemoteDevicePayload {
            device_id: id.into(),
            device_name: "Current name".into(),
            device_model: String::new(),
            device_type: "phone".into(),
            brand: String::new(),
            os_version: String::new(),
            battery_level: 79,
            battery_charging: false,
            connect_type: "wlan".into(),
            lan_ip: "192.168.1.2:10150".into(),
            local_ip: "192.168.1.1:0".into(),
            device_address: "192.168.1.2".into(),
            support_link_change: true,
        }
    }

    #[test]
    fn mixed_poll_uses_final_identity_and_reports_secondary_changes() {
        let mut state = AppState::new();
        let mut a = peer("A");
        let b = peer("B");
        let poll = |s: &mut AppState, list| s.reconcile_devices(Some(list), Ok(Some(remote("B"))));
        assert!(poll(&mut state, vec![b.clone()]));
        assert!(std::mem::take(&mut state.known_devices_dirty));
        assert!(poll(&mut state, vec![a.clone(), b.clone()]));
        assert!(std::mem::take(&mut state.known_devices_dirty));
        assert_eq!(state.connected_device.as_ref().unwrap().device_id, "B");
        assert!(!poll(&mut state, vec![a.clone(), b.clone()]));
        assert!(!state.known_devices_dirty);

        a.battery = 65;
        a.is_charging = true;
        assert!(poll(&mut state, vec![a.clone(), b.clone()]));
        assert!(!state.known_devices_dirty);
        a.device_type = "tablet".into();
        assert!(poll(&mut state, vec![a.clone(), b]));
        assert!(std::mem::take(&mut state.known_devices_dirty));

        // B arrived after /devices was sampled. It must never flip offline
        // and back online during this poll, even in the persistence queue.
        assert!(!poll(&mut state, vec![a]));
        assert!(!state.known_devices_dirty);
        assert!(poll(&mut state, vec![]));
        assert!(state.known_devices_dirty);
        assert_eq!(state.known_devices[0].device_id, "B");
        assert!(!state.known_devices[1].is_connected);
    }

    #[test]
    fn failed_device_list_preserves_peers_but_empty_list_disconnects_them() {
        let mut state = AppState::new();
        state.reconcile_devices(Some(vec![peer("A"), peer("B")]), Ok(Some(remote("B"))));
        state.known_devices_dirty = false;
        assert!(!state.reconcile_devices(None, Err("IPC unavailable".into())));
        assert!(state.reconcile_devices(None, Ok(Some(remote("A")))));
        assert!(state.known_devices.iter().all(|d| d.is_connected));
        assert!(state.reconcile_devices(Some(vec![]), Err("IPC unavailable".into())));
        assert!(state.connected_device.is_none() && state.remote_details.is_none());
        assert!(state.known_devices.iter().all(|d| !d.is_connected));
    }

    #[test]
    fn list_and_remote_filter_the_local_identity_and_reconnect_address() {
        let local = LocalDeviceInfo {
            pdid: "HOST".into(),
            addresses: vec!["192.168.1.2".parse().unwrap()],
        };
        assert!(!local.sanitize_device(&mut peer("hOsT")));
        let mut payload = Some(remote("host"));
        local.sanitize_remote(&mut payload);
        assert!(payload.is_none());
        let mut payload = Some(remote("A"));
        let mut device = DeviceInfo::from_remote(payload.as_ref().unwrap());
        assert!(local.sanitize_device(&mut device));
        local.sanitize_remote(&mut payload);
        assert!(device.ip_address.is_none());
        assert_eq!(device, DeviceInfo::from_remote(&payload.unwrap()));
        device.ip_address = Some("192.168.1.3".into());
        assert!(local.sanitize_device(&mut device));
        assert_eq!(device.ip_address.as_deref(), Some("192.168.1.3"));
    }
}
