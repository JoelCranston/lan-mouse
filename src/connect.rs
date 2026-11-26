use crate::client::ClientManager;
use crate::config::local_commit;
use crate::crypto;
use lan_mouse_ipc::{ClientHandle, DEFAULT_PORT};
use lan_mouse_proto::{MAX_EVENT_SIZE, ProtoEvent};
use local_channel::mpsc::{Receiver, Sender, channel};
use rustls::pki_types::CertificateDer;
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    io,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    rc::Rc,
    sync::{Arc, OnceLock, RwLock},
    time::Duration,
};
use thiserror::Error;
use tokio::{
    net::UdpSocket,
    sync::Mutex,
    task::{JoinSet, spawn_local},
};
use webrtc_dtls::{
    config::{Config, ExtendedMasterSecretType},
    conn::DTLSConn,
    crypto::Certificate,
};
use webrtc_util::Conn;

#[derive(Debug, Error)]
pub(crate) enum LanMouseConnectionError {
    #[error(transparent)]
    Bind(#[from] io::Error),
    #[error(transparent)]
    Dtls(#[from] webrtc_dtls::Error),
    #[error(transparent)]
    Webrtc(#[from] webrtc_util::Error),
    #[error("not connected")]
    NotConnected,
    #[error("emulation is disabled on the target device")]
    TargetEmulationDisabled,
    #[error("Connection timed out")]
    Timeout,
    #[error("peer certificate `{0}` is not authorized")]
    Unauthorized(String),
}

const DEFAULT_CONNECTION_TIMEOUT: Duration = Duration::from_secs(5);

/// Accept the peer only if its certificate fingerprint has been authorized,
/// recording the fingerprint of a rejected certificate in `rejected`.
fn verify_peer(
    certs: &[Vec<u8>],
    authorized_keys: &RwLock<HashMap<String, String>>,
    rejected: &OnceLock<String>,
) -> Result<(), webrtc_dtls::Error> {
    let cert = certs.first().ok_or(webrtc_dtls::Error::ErrNoCertificates)?;
    let fingerprint = crypto::generate_fingerprint(cert);
    if authorized_keys
        .read()
        .expect("lock")
        .contains_key(&fingerprint)
    {
        Ok(())
    } else {
        let _ = rejected.set(fingerprint);
        Err(webrtc_dtls::Error::ErrVerifyDataMismatch)
    }
}

async fn connect(
    addr: SocketAddr,
    cert: Certificate,
    authorized_keys: Arc<RwLock<HashMap<String, String>>>,
) -> Result<(Arc<dyn Conn + Sync + Send>, SocketAddr), (SocketAddr, LanMouseConnectionError)> {
    log::info!("connecting to {addr} ...");
    // bind an ephemeral socket of the same address family as the target:
    // an IPv4 socket can not send to IPv6 addresses (and vice versa)
    let bind_addr: SocketAddr = match addr {
        SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
        SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    let conn = Arc::new(
        UdpSocket::bind(bind_addr)
            .await
            .map_err(|e| (addr, e.into()))?,
    );
    conn.connect(addr).await.map_err(|e| (addr, e.into()))?;
    // certificates are self-signed, so instead of verifying a chain we only
    // talk to peers whose fingerprint the user has authorized
    let rejected = Arc::new(OnceLock::new());
    let verify_peer_certificate = {
        let rejected = rejected.clone();
        Arc::new(move |certs: &[Vec<u8>], _: &[CertificateDer<'static>]| {
            verify_peer(certs, &authorized_keys, &rejected)
        })
    };
    let config = Config {
        certificates: vec![cert],
        server_name: "ignored".to_owned(),
        insecure_skip_verify: true,
        verify_peer_certificate: Some(verify_peer_certificate),
        extended_master_secret: ExtendedMasterSecretType::Require,
        ..Default::default()
    };
    let timeout = tokio::time::sleep(DEFAULT_CONNECTION_TIMEOUT);
    tokio::select! {
        _ = timeout => Err((addr, LanMouseConnectionError::Timeout)),
        result = DTLSConn::new(conn, config, true, None) => match result {
            Ok(dtls_conn) => Ok((Arc::new(dtls_conn), addr)),
            Err(e) => match rejected.get() {
                Some(fp) => Err((addr, LanMouseConnectionError::Unauthorized(fp.clone()))),
                None => Err((addr, e.into())),
            },
        }
    }
}

async fn connect_any(
    addrs: &[SocketAddr],
    cert: Certificate,
    authorized_keys: Arc<RwLock<HashMap<String, String>>>,
    rejected_tx: Sender<String>,
) -> Result<(Arc<dyn Conn + Send + Sync>, SocketAddr), LanMouseConnectionError> {
    let mut joinset = JoinSet::new();
    for &addr in addrs {
        joinset.spawn_local(connect(addr, cert.clone(), authorized_keys.clone()));
    }
    loop {
        match joinset.join_next().await {
            None => return Err(LanMouseConnectionError::NotConnected),
            Some(r) => match r.expect("join error") {
                Ok(conn) => return Ok(conn),
                Err((a, e)) => {
                    log::warn!("failed to connect to {a}: `{e}`");
                    if let LanMouseConnectionError::Unauthorized(fingerprint) = e {
                        rejected_tx.send(fingerprint).expect("channel closed");
                    }
                }
            },
        };
    }
}

pub(crate) struct LanMouseConnection {
    cert: Certificate,
    client_manager: ClientManager,
    authorized_keys: Arc<RwLock<HashMap<String, String>>>,
    conns: Rc<Mutex<HashMap<SocketAddr, Arc<dyn Conn + Send + Sync>>>>,
    connecting: Rc<Mutex<HashSet<ClientHandle>>>,
    recv_rx: Receiver<(ClientHandle, ProtoEvent)>,
    recv_tx: Sender<(ClientHandle, ProtoEvent)>,
    rejected_tx: Sender<String>,
    ping_response: Rc<RefCell<HashSet<SocketAddr>>>,
    /// Clients that dropped the connection right after we sent them a
    /// [`ProtoEvent::Hello`]. These run a build that predates Hello
    /// (e.g. the v0.11.0 release), whose listener tears the connection
    /// down on any event type it does not know, so we stop sending
    /// Hello to them for the rest of this process' lifetime.
    no_hello: Rc<RefCell<HashSet<ClientHandle>>>,
}

impl LanMouseConnection {
    pub(crate) fn new(
        cert: Certificate,
        client_manager: ClientManager,
        authorized_keys: Arc<RwLock<HashMap<String, String>>>,
        rejected_tx: Sender<String>,
    ) -> Self {
        let (recv_tx, recv_rx) = channel();
        Self {
            cert,
            client_manager,
            authorized_keys,
            rejected_tx,
            conns: Default::default(),
            connecting: Default::default(),
            recv_rx,
            recv_tx,
            ping_response: Default::default(),
            no_hello: Default::default(),
        }
    }

    pub(crate) async fn recv(&mut self) -> (ClientHandle, ProtoEvent) {
        self.recv_rx.recv().await.expect("channel closed")
    }

    pub(crate) async fn send(
        &self,
        event: ProtoEvent,
        handle: ClientHandle,
    ) -> Result<(), LanMouseConnectionError> {
        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = event.into();
        let buf = &buf[..len];
        if let Some(addr) = self.client_manager.active_addr(handle) {
            let conn = {
                let conns = self.conns.lock().await;
                conns.get(&addr).cloned()
            };
            if let Some(conn) = conn {
                if !self.client_manager.alive(handle) {
                    return Err(LanMouseConnectionError::TargetEmulationDisabled);
                }
                match conn.send(buf).await {
                    Ok(_) => {}
                    Err(e) => {
                        log::warn!("client {handle} failed to send: {e}");
                        disconnect(&self.client_manager, handle, addr, &self.conns).await;
                    }
                }
                log::trace!("{event} >->->->->- {addr}");
                return Ok(());
            }
        }

        // check if we are already trying to connect
        let mut connecting = self.connecting.lock().await;
        if !connecting.contains(&handle) {
            connecting.insert(handle);
            // connect in the background
            spawn_local(connect_to_handle(
                self.client_manager.clone(),
                self.cert.clone(),
                self.authorized_keys.clone(),
                handle,
                self.conns.clone(),
                self.connecting.clone(),
                self.recv_tx.clone(),
                self.rejected_tx.clone(),
                self.ping_response.clone(),
                self.no_hello.clone(),
            ));
        }
        Err(LanMouseConnectionError::NotConnected)
    }
}

#[allow(clippy::too_many_arguments)]
async fn connect_to_handle(
    client_manager: ClientManager,
    cert: Certificate,
    authorized_keys: Arc<RwLock<HashMap<String, String>>>,
    handle: ClientHandle,
    conns: Rc<Mutex<HashMap<SocketAddr, Arc<dyn Conn + Send + Sync>>>>,
    connecting: Rc<Mutex<HashSet<ClientHandle>>>,
    tx: Sender<(ClientHandle, ProtoEvent)>,
    rejected_tx: Sender<String>,
    ping_response: Rc<RefCell<HashSet<SocketAddr>>>,
    no_hello: Rc<RefCell<HashSet<ClientHandle>>>,
) -> Result<(), LanMouseConnectionError> {
    log::info!("client {handle} connecting ...");
    // sending did not work, figure out active conn.
    if let Some(addrs) = client_manager.get_ips(handle) {
        let port = client_manager.get_port(handle).unwrap_or(DEFAULT_PORT);
        let addrs = addrs
            .into_iter()
            .map(|a| SocketAddr::new(a, port))
            .collect::<Vec<_>>();
        log::info!("client ({handle}) connecting ... (ips: {addrs:?})");
        let res = connect_any(&addrs, cert, authorized_keys, rejected_tx).await;
        let (conn, addr) = match res {
            Ok(c) => c,
            Err(e) => {
                connecting.lock().await.remove(&handle);
                return Err(e);
            }
        };
        log::info!("client ({handle}) connected @ {addr}");
        client_manager.set_active_addr(handle, Some(addr));
        conns.lock().await.insert(addr, conn.clone());
        connecting.lock().await.remove(&handle);

        // poll connection for active
        spawn_local(ping_pong(addr, conn.clone(), ping_response.clone()));

        // receiver
        spawn_local(receive_loop(
            client_manager,
            handle,
            addr,
            conn,
            conns,
            tx,
            ping_response.clone(),
            no_hello,
        ));
        return Ok(());
    }
    connecting.lock().await.remove(&handle);
    Err(LanMouseConnectionError::NotConnected)
}

async fn ping_pong(
    addr: SocketAddr,
    conn: Arc<dyn Conn + Send + Sync>,
    ping_response: Rc<RefCell<HashSet<SocketAddr>>>,
) {
    loop {
        let (buf, len) = ProtoEvent::Ping.into();

        // send 4 pings, at least one must be answered
        for _ in 0..4 {
            if let Err(e) = conn.send(&buf[..len]).await {
                log::warn!("{addr}: send error `{e}`, closing connection");
                let _ = conn.close().await;
                break;
            }
            log::trace!("PING >->->->->- {addr}");

            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        if !ping_response.borrow_mut().remove(&addr) {
            log::warn!("{addr} did not respond, closing connection");
            let _ = conn.close().await;
            return;
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn receive_loop(
    client_manager: ClientManager,
    handle: ClientHandle,
    addr: SocketAddr,
    conn: Arc<dyn Conn + Send + Sync>,
    conns: Rc<Mutex<HashMap<SocketAddr, Arc<dyn Conn + Send + Sync>>>>,
    tx: Sender<(ClientHandle, ProtoEvent)>,
    ping_response: Rc<RefCell<HashSet<SocketAddr>>>,
    no_hello: Rc<RefCell<HashSet<ClientHandle>>>,
) {
    let mut buf = [0u8; MAX_EVENT_SIZE];
    let mut hello = HelloProbe::new(!no_hello.borrow().contains(&handle));
    while conn.recv(&mut buf).await.is_ok() {
        match buf.try_into() {
            Ok(event) => {
                log::trace!("{addr} <==<==<== {event}");
                match event {
                    ProtoEvent::Pong(b) => {
                        client_manager.set_active_addr(handle, Some(addr));
                        client_manager.set_alive(handle, b);
                        ping_response.borrow_mut().insert(addr);
                        if hello.on_pong() {
                            send_hello(&*conn, addr).await;
                        }
                    }
                    ProtoEvent::Hello { commit } => {
                        hello.on_hello();
                        client_manager.set_peer_commit(handle, Some(commit));
                    }
                    event => tx.send((handle, event)).expect("channel closed"),
                }
            }
            // Skip undecodable datagrams without dropping the
            // connection. Each DTLS recv is one framed message, so
            // skipping is safe and keeps us forward-compatible with
            // peers that send event types we don't yet know about.
            Err(e) => log::debug!("ignoring undecodable event from {addr}: {e}"),
        }
    }
    log::warn!("recv error");
    if hello.peer_rejected_hello() {
        log::info!(
            "client ({handle}) @ {addr} closed the connection right after our Hello; \
             assuming it runs an older Lan Mouse (e.g. v0.11.0) and not sending Hello to it again"
        );
        no_hello.borrow_mut().insert(handle);
    }
    disconnect(&client_manager, handle, addr, &conns).await;
}

/// Best-effort version handshake: send our commit hash so the peer can
/// show it. A peer that knows Hello mirrors one back.
async fn send_hello(conn: &(dyn Conn + Send + Sync), addr: SocketAddr) {
    let (buf, len) = ProtoEvent::Hello {
        commit: local_commit(),
    }
    .into();
    if let Err(e) = conn.send(&buf[..len]).await {
        log::debug!("hello send to {addr} failed: {e}");
    }
}

/// Decides when to send [`ProtoEvent::Hello`] on an outgoing connection
/// and whether the peer turned out to be too old for it.
///
/// Builds before Hello (such as the v0.11.0 release) close the connection
/// when they receive an event type they don't know, so Hello is probed
/// carefully: it is sent only after the first `Pong`, which proves the
/// peer is up and answering. If the connection then ends before any
/// further `Pong` (or a Hello reply) arrives, the peer dropped it because
/// of the Hello, and the caller stops sending Hello to it. Pings go out
/// every 500ms, so on a working peer the next `Pong` follows quickly.
struct HelloProbe {
    enabled: bool,
    sent: bool,
    confirmed: bool,
}

impl HelloProbe {
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            sent: false,
            confirmed: false,
        }
    }

    /// Returns true when Hello should be sent now.
    fn on_pong(&mut self) -> bool {
        if self.sent {
            self.confirmed = true;
            false
        } else if self.enabled {
            self.sent = true;
            true
        } else {
            false
        }
    }

    fn on_hello(&mut self) {
        self.confirmed = true;
    }

    /// True if the connection ended after Hello was sent and before the
    /// peer showed it was still talking to us.
    fn peer_rejected_hello(&self) -> bool {
        self.sent && !self.confirmed
    }
}

async fn disconnect(
    client_manager: &ClientManager,
    handle: ClientHandle,
    addr: SocketAddr,
    conns: &Mutex<HashMap<SocketAddr, Arc<dyn Conn + Send + Sync>>>,
) {
    log::warn!("client ({handle}) @ {addr} connection closed");
    conns.lock().await.remove(&addr);
    client_manager.set_active_addr(handle, None);
    client_manager.set_peer_commit(handle, None);
    let active: Vec<SocketAddr> = conns.lock().await.keys().copied().collect();
    log::info!("active connections: {active:?}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer_cert() -> Vec<u8> {
        let cert =
            Certificate::generate_self_signed(vec!["ignored".to_owned()]).expect("certificate");
        cert.certificate[0].to_vec()
    }

    #[test]
    fn verify_peer_accepts_authorized_fingerprint() {
        let cert = peer_cert();
        let fingerprint = crypto::generate_fingerprint(&cert);
        let authorized = RwLock::new(HashMap::from([(fingerprint, "peer".to_owned())]));
        let rejected = OnceLock::new();
        assert!(verify_peer(&[cert], &authorized, &rejected).is_ok());
        assert!(rejected.get().is_none());
    }

    #[test]
    fn verify_peer_rejects_unknown_fingerprint() {
        let cert = peer_cert();
        let fingerprint = crypto::generate_fingerprint(&cert);
        let authorized = RwLock::new(HashMap::new());
        let rejected = OnceLock::new();
        assert!(verify_peer(&[cert], &authorized, &rejected).is_err());
        assert_eq!(rejected.get(), Some(&fingerprint));
    }

    #[test]
    fn hello_probe_sends_once_after_first_pong() {
        let mut probe = HelloProbe::new(true);
        assert!(probe.on_pong());
        assert!(!probe.on_pong());
        assert!(!probe.on_pong());
        assert!(!probe.peer_rejected_hello());
    }

    #[test]
    fn hello_probe_detects_old_peer() {
        // v0.11.0: answers pings until it sees Hello, then drops the connection
        let mut probe = HelloProbe::new(true);
        assert!(probe.on_pong());
        assert!(probe.peer_rejected_hello());
    }

    #[test]
    fn hello_probe_hello_reply_confirms() {
        let mut probe = HelloProbe::new(true);
        assert!(probe.on_pong());
        probe.on_hello();
        assert!(!probe.peer_rejected_hello());
    }

    #[test]
    fn hello_probe_never_pinged_is_not_old_peer() {
        // connection died before the peer ever answered: not Hello's fault
        let probe = HelloProbe::new(true);
        assert!(!probe.peer_rejected_hello());
    }

    #[test]
    fn hello_probe_disabled_never_sends() {
        let mut probe = HelloProbe::new(false);
        assert!(!probe.on_pong());
        assert!(!probe.on_pong());
        assert!(!probe.peer_rejected_hello());
    }

    /// Minimal DTLS listener that answers `Ping` with `Pong` and, like the
    /// listener in older releases, optionally drops the connection on any
    /// event it can't decode. Counts the Hellos it receives.
    async fn mock_peer(cert: Certificate, knows_hello: bool, hellos: Rc<RefCell<usize>>) -> u16 {
        use webrtc_util::conn::Listener;
        let cfg = Config {
            certificates: vec![cert],
            extended_master_secret: ExtendedMasterSecretType::Require,
            ..Default::default()
        };
        let listener = webrtc_dtls::listener::listen("127.0.0.1:0", cfg)
            .await
            .expect("listen");
        let port = listener.addr().await.expect("addr").port();
        spawn_local(async move {
            loop {
                let Ok((conn, _)) = listener.accept().await else {
                    return;
                };
                let hellos = hellos.clone();
                spawn_local(async move {
                    let mut buf = [0u8; MAX_EVENT_SIZE];
                    while conn.recv(&mut buf).await.is_ok() {
                        let event = buf.try_into();
                        if let Ok(ProtoEvent::Hello { .. }) = event {
                            *hellos.borrow_mut() += 1;
                            if !knows_hello {
                                // v0.11.0 `read_loop`: `break` on decode error
                                return;
                            }
                            let (b, l) = ProtoEvent::Hello {
                                commit: *b"mockpeer",
                            }
                            .into();
                            let _ = conn.send(&b[..l]).await;
                            continue;
                        }
                        if let Ok(ProtoEvent::Ping) = event {
                            let (b, l) = ProtoEvent::Pong(true).into();
                            let _ = conn.send(&b[..l]).await;
                        }
                    }
                });
            }
        });
        port
    }

    /// Connects to a mock peer and keeps sending until the connection has
    /// stayed usable for a while. Returns (hellos seen by peer, peer commit).
    async fn connect_to_mock(knows_hello: bool) -> (usize, Option<[u8; 8]>) {
        let server_cert =
            Certificate::generate_self_signed(vec!["ignored".to_owned()]).expect("certificate");
        let fingerprint = crypto::generate_fingerprint(&server_cert.certificate[0]);
        let hellos = Rc::new(RefCell::new(0));
        let port = mock_peer(server_cert, knows_hello, hellos.clone()).await;

        let client_manager = ClientManager::default();
        let handle = client_manager.add_client();
        client_manager.set_fix_ips(handle, vec!["127.0.0.1".parse().unwrap()]);
        client_manager.set_port(handle, port);
        let authorized = Arc::new(RwLock::new(HashMap::from([(fingerprint, "p".into())])));
        let (rejected_tx, _rejected_rx) = channel();
        let cert =
            Certificate::generate_self_signed(vec!["ignored".to_owned()]).expect("certificate");
        let conn = LanMouseConnection::new(cert, client_manager.clone(), authorized, rejected_tx);

        // require ~4s (several ping rounds) of uninterrupted connectivity
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        let mut ok_streak = 0;
        while ok_streak < 40 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "connection never became stable"
            );
            match conn.send(ProtoEvent::Leave(0), handle).await {
                Ok(()) => ok_streak += 1,
                Err(_) => ok_streak = 0,
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let commit = client_manager
            .get_state(handle)
            .and_then(|(_, s)| s.peer_commit);
        let hellos = *hellos.borrow();
        (hellos, commit)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn connects_to_peer_without_hello_support() {
        let local = tokio::task::LocalSet::new();
        let (hellos, commit) = local.run_until(connect_to_mock(false)).await;
        // probed exactly once, then the reconnect leaves Hello out
        assert_eq!(hellos, 1);
        assert_eq!(commit, None);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn exchanges_hello_with_new_peer() {
        let local = tokio::task::LocalSet::new();
        let (hellos, commit) = local.run_until(connect_to_mock(true)).await;
        assert_eq!(hellos, 1);
        assert_eq!(commit, Some(*b"mockpeer"));
    }

    #[test]
    fn verify_peer_rejects_missing_certificate() {
        let authorized = RwLock::new(HashMap::new());
        let rejected = OnceLock::new();
        assert!(verify_peer(&[], &authorized, &rejected).is_err());
    }
}
