//! SDP (Service Discovery Protocol) announce sender.
//!
//! Packet shape is an HTTP/1.1-style message carried over UDP and parsed by the
//! phone's `libcpkit.so` (`discovery::sdp::lan`, method table
//! `ANNOUNCE/QUERY/GOODBYE/SENSELESS/SENSELESS-ACK/SERVICE-PUBLISH/SERVICE-QUERY/SERVICE-ACK`).
//!
//! Verified on a ColorOS 17 tablet (OPPO Pad Mini, PDID 20108139A722): the exact
//! packet below makes the phone's PTC discovery scheduler register
//!
//! ```text
//! DisplayDevice(dvd=0F0B, deviceType=PC, deviceState=DISCOVERED,
//!               socketPort=10150, deviceIp=192.168.2.207,
//!               discoveryStrategy=lan, accountDigest=456564)
//! ```
//!
//! and the device shows up in the tablet's "附近的设备" list.
//!
//! ```text
//! ANNOUNCE / HTTP/1.1
//! MAN: "sdp/1"          <- protocol_name "sdp", protocol_version u16 == 1
//! NS: sdp/announce      <- namespace (must stay a plain namespace: "scp.device"
//!                          makes the generic discovery log
//!                          "Invalid service id format")
//! DSP: 1
//! CSP: 10150            <- TCP port the phone dials back on (DisplayDevice.socketPort)
//! DMS: 1
//! PDID: <hex>           <- protocol device id
//! AD: <short hex>       <- account digest; any value works for a fresh pairing
//! DT: 6                 <- device type (6 = PC)
//! DN: <string>          <- display name
//! ```
//!
//! SDP datagrams use source port 10151 and target `255.255.255.250:10150`, each private interface's directed broadcast, and a known peer's unicast address.

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tracing::{info, warn};

pub const SDP_ANNOUNCE_PORT: u16 = 10150;
pub const SDP_SOURCE_PORT: u16 = 10151;
pub const SDP_MULTICAST: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
/// SSDP-style broadcast address the official daemon publishes to.
pub const SDP_BROADCAST: Ipv4Addr = Ipv4Addr::new(255, 255, 255, 250);

/// Device type enum values observed on the phone side.
pub const DEVICE_TYPE_PC: u16 = 6;

/// Builds the native Windows-style SDP announce used both for UDP responses
/// and for the phone's TCP `QUERY * HTTP/1.1` probe.
pub fn announce_payload(connect_port: u16) -> String {
    let pdid = crate::net_utils::pdid();
    let display_name = std::env::var("OPLUS_DN").unwrap_or_else(|_| crate::net_utils::hostname());
    let account_digest = std::env::var("OPLUS_AD").unwrap_or_else(|_| "456564".to_string());
    let virtual_ip = std::env::var("OPLUS_VIRTUAL_IP")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(local_ipv4)
        .unwrap_or_else(|| "127.0.0.1".to_string());
    format!(
        concat!(
            "ANNOUNCE * HTTP/1.1\r\n",
            "MAN: \"sdp/1\"\r\n",
            "NS: scp.device\r\n",
            "DSP: 10151\r\n",
            "CSP: {port}\r\n",
            "DMS: 0\r\n",
            "PDID: {pdid}\r\n",
            "AD: {ad}\r\n",
            "DT: {dt}\r\n",
            "DN: {name}\r\n",
            "VIRTUAL_IP: {virtual_ip}\r\n",
            "NSDATA: \r\n",
            "\r\n"
        ),
        port = connect_port,
        pdid = pdid.to_uppercase(),
        ad = account_digest,
        dt = DEVICE_TYPE_PC,
        name = display_name,
        virtual_ip = virtual_ip,
    )
}

pub struct Announcer {
    socket: UdpSocket,
    query_socket: UdpSocket,
    pdid: String,
    display_name: String,
    connect_port: u16,
    account_digest: String,
    official_sdp: bool,
    virtual_ip: String,
    private_broadcasts: Vec<Ipv4Addr>,
}

/// Owns the blocking UDP worker. Dropping this handle requests cancellation;
/// shutdown additionally waits until both discovery sockets have been released.
#[must_use = "the announcer stops when its task handle is dropped"]
pub struct AnnouncerTask {
    cancelled: Arc<AtomicBool>,
    worker: Option<tokio::task::JoinHandle<()>>,
}

impl AnnouncerTask {
    pub async fn shutdown(mut self) -> Result<(), tokio::task::JoinError> {
        self.cancelled.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.await?;
        }
        Ok(())
    }
}

impl Drop for AnnouncerTask {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

impl Announcer {
    pub fn new(connect_port: u16) -> std::io::Result<Self> {
        // The receiver uses source port 10151 to recognize the official SDP
        // announcement. An ephemeral-port fallback looks successful but is ignored.
        let socket = UdpSocket::bind(("0.0.0.0", SDP_SOURCE_PORT))?;
        socket.set_broadcast(true)?;
        socket.set_write_timeout(Some(Duration::from_millis(500)))?;

        // PLZ110 sends a multicast QUERY and only processes the unicast
        // ANNOUNCE response. Unsolicited announcements are rejected by its
        // generic discovery dispatcher.
        let query_socket = UdpSocket::bind(("0.0.0.0", SDP_ANNOUNCE_PORT))?;
        let _ = query_socket.join_multicast_v4(&SDP_MULTICAST, &Ipv4Addr::UNSPECIFIED);
        for iface_ip in crate::net_utils::private_interface_ips() {
            let _ = query_socket.join_multicast_v4(&SDP_MULTICAST, &iface_ip);
        }
        query_socket.set_read_timeout(Some(Duration::from_millis(100)))?;

        // Overridable so each protocol experiment can present a fresh identity
        // (the phone caches peers by PDID and would otherwise skip re-dialling).
        let pdid = crate::net_utils::pdid();
        let display_name =
            std::env::var("OPLUS_DN").unwrap_or_else(|_| crate::net_utils::hostname());
        let private_broadcasts = crate::net_utils::private_broadcast_ips();

        // Short digest, matching the shape observed from the official daemon
        // (`AD: 456564` / phone's `AD: 989A18`).
        let account_digest = std::env::var("OPLUS_AD").unwrap_or_else(|_| "456564".to_string());
        // The phone's own QUERY→ANNOUNCE path expects the native `scp.device`
        // announcement; `OPLUS_SDP_LEGACY=1` falls back to the old namespace.
        let official_sdp = std::env::var("OPLUS_SDP_LEGACY").as_deref() != Ok("1");
        let virtual_ip = std::env::var("OPLUS_VIRTUAL_IP")
            .ok()
            .filter(|value| !value.is_empty())
            .or_else(local_ipv4)
            .unwrap_or_else(|| "127.0.0.1".to_string());

        Ok(Self {
            socket,
            query_socket,
            pdid,
            display_name,
            connect_port,
            private_broadcasts,
            account_digest,
            official_sdp,
            virtual_ip,
        })
    }

    pub fn payload(&self) -> String {
        if self.official_sdp {
            // PLZ110 rejects the old tablet namespace as an unsupported LAN
            // packet. This is the native Windows scp.device announcement.
            return format!(
                concat!(
                    "ANNOUNCE * HTTP/1.1\r\n",
                    "MAN: \"sdp/1\"\r\n",
                    "NS: scp.device\r\n",
                    "DSP: 10151\r\n",
                    "CSP: {port}\r\n",
                    "DMS: 0\r\n",
                    "PDID: {pdid}\r\n",
                    "AD: {ad}\r\n",
                    "DT: {dt}\r\n",
                    "DN: {name}\r\n",
                    "VIRTUAL_IP: {virtual_ip}\r\n",
                    "NSDATA: \r\n",
                    "\r\n"
                ),
                port = self.connect_port,
                pdid = self.pdid.to_uppercase(),
                ad = self.account_digest,
                dt = DEVICE_TYPE_PC,
                name = self.display_name,
                virtual_ip = self.virtual_ip,
            );
        }

        // Byte-identical to the packet the tablet accepted (see module docs).
        format!(
            concat!(
                "ANNOUNCE / HTTP/1.1\r\n",
                "MAN: \"sdp/1\"\r\n",
                "NS: sdp/announce\r\n",
                "DSP: 1\r\n",
                "CSP: {port}\r\n",
                "DMS: 1\r\n",
                "PDID: {pdid}\r\n",
                "AD: {ad}\r\n",
                "DT: {dt}\r\n",
                "DN: {name}\r\n",
                "\r\n"
            ),
            port = self.connect_port,
            pdid = self.pdid,
            ad = self.account_digest,
            dt = DEVICE_TYPE_PC,
            name = self.display_name,
        )
    }

    /// Publishes via SSDP, every detected private subnet, and a known peer.
    pub fn announce_once(&self, peer: Option<Ipv4Addr>) {
        let payload = self.payload();
        let bytes = payload.as_bytes();

        for port in [SDP_ANNOUNCE_PORT, SDP_SOURCE_PORT] {
            let bcast = SocketAddr::from((SDP_BROADCAST, port));
            if let Err(e) = self.socket.send_to(bytes, bcast) {
                warn!("sdp announce broadcast {} failed: {}", bcast, e);
            }
            for ip in &self.private_broadcasts {
                let directed = SocketAddr::from((*ip, port));
                if let Err(e) = self.socket.send_to(bytes, directed) {
                    warn!("sdp directed broadcast {} failed: {}", directed, e);
                }
            }
        }
        if let Some(ip) = peer {
            for port in [SDP_ANNOUNCE_PORT, SDP_SOURCE_PORT] {
                let unicast = SocketAddr::from((ip, port));
                if let Err(e) = self.socket.send_to(bytes, unicast) {
                    warn!("sdp announce unicast {} failed: {}", unicast, e);
                }
            }
        }
    }

    pub fn spawn(
        self,
        peer_provider: impl Fn() -> Option<Ipv4Addr> + Send + 'static,
    ) -> AnnouncerTask {
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = cancelled.clone();
        let worker = tokio::task::spawn_blocking(move || {
            info!("sdp announcer started (DT=PC, CSP={})", self.connect_port);
            info!("sdp announce payload: {:?}", self.payload());
            let mut last_announce = std::time::Instant::now() - Duration::from_secs(2);
            let mut recv_buf = [0u8; 2048];
            while !worker_cancelled.load(Ordering::Acquire) {
                match self.query_socket.recv_from(&mut recv_buf) {
                    Ok((size, source)) => {
                        let query = &recv_buf[..size];
                        if query.starts_with(b"QUERY ")
                            && query
                                .windows(b"NS: scp.device".len())
                                .any(|part| part == b"NS: scp.device")
                        {
                            info!("sdp query from {}", source);
                            // The reply must leave from the official source port
                            // 10151; an ephemeral port is ignored by the phone.
                            let response = self.payload();
                            let destination = SocketAddr::from((source.ip(), source.port()));
                            if let Err(error) =
                                self.socket.send_to(response.as_bytes(), destination)
                            {
                                warn!("sdp query response {} failed: {}", destination, error);
                            } else {
                                info!("sdp query response sent to {}", destination);
                            }
                        }
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) => {}
                    Err(error) => warn!("sdp query receive failed: {}", error),
                }

                if worker_cancelled.load(Ordering::Acquire) {
                    break;
                }
                if last_announce.elapsed() >= Duration::from_secs(2) {
                    self.announce_once(peer_provider());
                    last_announce = std::time::Instant::now();
                }
            }
            // Release resources before completing the JoinHandle, including when
            // cancellation arrives before the first receive iteration.
            drop(self);
        });
        AnnouncerTask {
            cancelled,
            worker: Some(worker),
        }
    }
}

fn local_ipv4() -> Option<String> {
    let socket = UdpSocket::bind(("0.0.0.0", 0)).ok()?;
    socket.connect(("192.168.2.1", 9)).ok()?;
    socket.local_addr().ok().map(|addr| addr.ip().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn announcer() -> Announcer {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let query_socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        query_socket
            .set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        Announcer {
            socket,
            query_socket,
            pdid: "abcdef012345".into(),
            display_name: "SDP lifecycle test".into(),
            connect_port: 10150,
            account_digest: "456564".into(),
            official_sdp: true,
            virtual_ip: "127.0.0.1".into(),
            private_broadcasts: Vec::new(),
        }
    }

    async fn rebind_after_shutdown(address: SocketAddr) {
        // Concurrent process-spawn tests can briefly inherit CLOEXEC sockets
        // across fork. A real leak must still fail within the shutdown bound.
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match UdpSocket::bind(address) {
                    Ok(socket) => break socket,
                    Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                    Err(error) => panic!("cannot rebind {address}: {error}"),
                }
            }
        })
        .await
        .expect("discovery socket was not released");
    }

    #[tokio::test]
    async fn shutdown_joins_the_worker_and_releases_both_sockets() {
        let announcer = announcer();
        let source = announcer.socket.local_addr().unwrap();
        let query = announcer.query_socket.local_addr().unwrap();
        let client = tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let task = announcer.spawn(|| None);
        client
            .send_to(b"QUERY * HTTP/1.1\r\nNS: scp.device\r\n\r\n", query)
            .await
            .unwrap();
        let mut response = [0u8; 2048];
        let (size, sender) =
            tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut response))
                .await
                .unwrap()
                .unwrap();
        assert!(response[..size].starts_with(b"ANNOUNCE "));
        assert_eq!(sender, source);

        tokio::time::timeout(Duration::from_secs(2), task.shutdown())
            .await
            .unwrap()
            .unwrap();
        rebind_after_shutdown(source).await;
        rebind_after_shutdown(query).await;
    }

    #[tokio::test]
    async fn dropped_handle_cancels_the_blocking_worker() {
        let announcer = announcer();
        let query = announcer.query_socket.local_addr().unwrap();
        let mut task = announcer.spawn(|| None);
        let worker = task.worker.take().unwrap();
        drop(task);
        tokio::time::timeout(Duration::from_secs(2), worker)
            .await
            .unwrap()
            .unwrap();
        rebind_after_shutdown(query).await;
    }
}
