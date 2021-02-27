use std::{
    fs::File,
    io::{self, Read},
    sync::Arc,
};

use log::LevelFilter;
use serde::Deserialize;
use tokio::io::{split, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::{
    error::Error,
    protocol::{
        direct::connector::DirectConnector,
        dokodemo::acceptor::{DokodemoAcceptor, DokodemoAcceptorConfig},
        socks5::acceptor::{Socks5Acceptor, Socks5AcceptorConfig},
        tls::{
            acceptor::{TrojanTlsAcceptor, TrojanTlsAcceptorConfig},
            connector::{TrojanTlsConnector, TrojanTlsConnectorConfig},
        },
        trojan::{
            acceptor::{TrojanAcceptor, TrojanAcceptorConfig},
            connector::{TrojanConnector, TrojanConnectorConfig},
        },
        websocket::{
            acceptor::{WebSocketAcceptor, WebSocketAcceptorConfig},
            connector::{WebSocketConnector, WebSocketConnectorConfig},
        },
        AcceptResult, ProxyAcceptor, ProxyConnector, ProxyTcpStream, ProxyUdpStream, UdpRead,
        UdpWrite,
    },
};

async fn copy_udp<R: UdpRead, W: UdpWrite>(r: &mut R, w: &mut W) -> io::Result<()> {
    let mut buf = [0u8; 1024 * 8];
    loop {
        let (size, addr) = r.read_from(&mut buf).await?;
        if size == 0 {
            break;
        }
        w.write_to(&buf[..size], &addr).await?;
    }
    Ok(())
}

async fn copy_tcp<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    r: &mut R,
    w: &mut W,
) -> io::Result<()> {
    let mut buf = [0u8; 1024 * 32];
    loop {
        let size = r.read(&mut buf).await?;
        if size == 0 {
            break;
        }
        w.write_all(&buf[..size]).await?;
    }
    Ok(())
}

pub async fn relay_udp<T: ProxyUdpStream, U: ProxyUdpStream>(a: T, b: U) {
    let (mut a_rx, mut a_tx) = a.split();
    let (mut b_rx, mut b_tx) = b.split();
    let t1 = copy_udp(&mut a_rx, &mut b_tx);
    let t2 = copy_udp(&mut b_rx, &mut a_tx);
    let e = tokio::select! {
        e = t1 => {e}
        e = t2 => {e}
    };
    if let Err(e) = e {
        log::debug!("udp session ends: {}", e)
    }
    let _ = T::reunite(a_rx, a_tx).close();
    let _ = U::reunite(b_rx, b_tx).close();
}

pub async fn relay_tcp<T: ProxyTcpStream, U: ProxyTcpStream>(a: T, b: U) {
    let (mut a_rx, mut a_tx) = split(a);
    let (mut b_rx, mut b_tx) = split(b);
    let t1 = copy_tcp(&mut a_rx, &mut b_tx);
    let t2 = copy_tcp(&mut b_rx, &mut a_tx);
    let e = tokio::select! {
        e = t1 => {e}
        e = t2 => {e}
    };
    if let Err(e) = e {
        log::debug!("tcp session ends: {}", e)
    }
    let mut a = a_rx.unsplit(a_tx);
    let mut b = b_rx.unsplit(b_tx);
    let _ = a.shutdown().await;
    let _ = b.shutdown().await;
}

#[derive(Deserialize)]
struct GlobalConfig {
    mode: String,
    log_level: Option<String>,
}

#[derive(Deserialize)]
struct ClientConfig {
    socks5: Socks5AcceptorConfig,
    trojan: TrojanConnectorConfig,
    tls: TrojanTlsConnectorConfig,
    websocket: Option<WebSocketConnectorConfig>,
}

#[derive(Deserialize)]
struct ServerConfig {
    trojan: TrojanAcceptorConfig,
    tls: TrojanTlsAcceptorConfig,
    websocket: Option<WebSocketAcceptorConfig>,
}

#[derive(Deserialize)]
struct ForwardConfig {
    dokodemo: DokodemoAcceptorConfig,
    trojan: TrojanConnectorConfig,
    tls: TrojanTlsConnectorConfig,
    websocket: Option<WebSocketConnectorConfig>,
}

async fn run_proxy<I: ProxyAcceptor, O: ProxyConnector + 'static>(
    acceptor: I,
    connector: O,
) -> io::Result<()> {
    let connector = Arc::new(connector);
    loop {
        match acceptor.accept().await {
            Ok(AcceptResult::Tcp((inbound, addr))) => {
                let connector = connector.clone();
                tokio::spawn(async move {
                    match connector.connect_tcp(&addr).await {
                        Ok(outbound) => {
                            relay_tcp(inbound, outbound).await;
                        }
                        Err(e) => {
                            log::error!(
                                "failed to relay tcp connection to {}: {}",
                                addr.to_string(),
                                e.to_string()
                            );
                        }
                    }
                });
            }
            Ok(AcceptResult::Udp(inbound)) => {
                let connector = connector.clone();
                tokio::spawn(async move {
                    match connector.connect_udp().await {
                        Ok(outbound) => {
                            relay_udp(inbound, outbound).await;
                        }
                        Err(e) => {
                            log::error!("failed to relay tcp connection: {}", e.to_string());
                        }
                    }
                });
            }
            Err(e) => {
                log::error!("accept failed: {}", e);
            }
        }
    }
}

pub async fn launch_from_config_filename(filename: String) -> io::Result<()> {
    let mut file = File::open(filename)?;
    let mut config_string = String::new();
    file.read_to_string(&mut config_string)?;
    launch_from_config_string(config_string).await
}

pub async fn launch_from_config_string(config_string: String) -> io::Result<()> {
    let config: GlobalConfig = toml::from_str(&config_string)?;
    if let Some(log_level) = config.log_level {
        let level = match log_level.as_str() {
            "trace" => LevelFilter::Trace,
            "debug" => LevelFilter::Debug,
            "info" => LevelFilter::Info,
            "warn" => LevelFilter::Warn,
            "error" => LevelFilter::Error,
            _ => {
                return Err(Error::new("invalid log_level").into());
            }
        };
        let _ = env_logger::builder().filter_level(level).try_init();
    } else {
        let _ = env_logger::builder()
            .filter_level(LevelFilter::Debug)
            .try_init();
    }
    match config.mode.as_str() {
        "server" => {
            log::debug!("server mode");
            let config: ServerConfig = toml::from_str(&config_string)?;
            let direct_connector = DirectConnector {};
            let tls_acceptor = TrojanTlsAcceptor::new(&config.tls).await?;
            if config.websocket.is_none() {
                let trojan_acceptor = TrojanAcceptor::new(&config.trojan, tls_acceptor)?;
                run_proxy(trojan_acceptor, direct_connector).await?;
            } else {
                let ws_acceptor = WebSocketAcceptor::new(&config.websocket.unwrap(), tls_acceptor)?;
                let trojan_acceptor = TrojanAcceptor::new(&config.trojan, ws_acceptor)?;
                run_proxy(trojan_acceptor, direct_connector).await?;
            }
        }
        "client" => {
            log::debug!("client mode");
            let config: ClientConfig = toml::from_str(&config_string)?;
            let socks5_acceptor = Socks5Acceptor::new(&config.socks5).await?;
            let tls_connector = TrojanTlsConnector::new(&config.tls)?;
            if config.websocket.is_none() {
                let trojan_connector = TrojanConnector::new(&config.trojan, tls_connector)?;
                run_proxy(socks5_acceptor, trojan_connector).await?;
            } else {
                let ws_connector =
                    WebSocketConnector::new(&config.websocket.unwrap(), tls_connector)?;
                let trojan_connector = TrojanConnector::new(&config.trojan, ws_connector)?;
                run_proxy(socks5_acceptor, trojan_connector).await?;
            }
        }
        "forward" => {
            log::debug!("forward mode");
            let config: ForwardConfig = toml::from_str(&config_string)?;
            let dokodemo_acceptor = DokodemoAcceptor::new(&config.dokodemo).await?;
            let tls_connector = TrojanTlsConnector::new(&config.tls)?;
            if config.websocket.is_none() {
                let trojan_connector = TrojanConnector::new(&config.trojan, tls_connector)?;
                run_proxy(dokodemo_acceptor, trojan_connector).await?;
            } else {
                let ws_connector =
                    WebSocketConnector::new(&config.websocket.unwrap(), tls_connector)?;
                let trojan_connector = TrojanConnector::new(&config.trojan, ws_connector)?;
                run_proxy(dokodemo_acceptor, trojan_connector).await?;
            }
        }
        _ => {
            log::error!("invalid mode: {}", config.mode.as_str());
        }
    }
    Ok(())
}
