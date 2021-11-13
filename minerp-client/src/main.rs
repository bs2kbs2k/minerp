#![feature(never_type)]
#![allow(dead_code)]

use lazy_static::lazy_static;
use minerp_common::*;
use tokio_rustls::rustls;

lazy_static! {
    static ref CONN_CONF: std::sync::Arc<rustls::ClientConfig> = {
        let mut root_store = rustls::RootCertStore::empty();
        root_store.add_server_trust_anchors(webpki_roots::TLS_SERVER_ROOTS.0.iter().map(|ta| {
            rustls::OwnedTrustAnchor::from_subject_spki_name_constraints(
                ta.subject,
                ta.spki,
                ta.name_constraints,
            )
        }));
        let conf = rustls::ClientConfig::builder()
            .with_safe_defaults()
            .with_root_certificates(root_store)
            .with_no_client_auth();
        std::sync::Arc::new(conf)
    };
}

#[tokio::main]
async fn main() {
    let args = std::env::args().collect::<Vec<_>>();
    let conn = ControlConnection::connect(
        args.get(1).unwrap().clone(),
        args.get(2).unwrap().parse().unwrap(),
        args.get(3).unwrap().parse().unwrap(),
    )
    .await
    .unwrap();
    conn.handle(args.get(4).cloned(), |domain| async move {
        println!("{}", domain);
        Ok(())
    })
    .await
    .unwrap();
}

async fn tls_connect(
    addr: &str,
    port: u16,
) -> anyhow::Result<tokio_rustls::client::TlsStream<tokio::net::TcpStream>> {
    let stream = tokio::net::TcpStream::connect((addr, port)).await?;
    let conf = tokio_rustls::TlsConnector::from(CONN_CONF.clone());
    let conn = conf
        .connect(
            rustls::ServerName::try_from(addr)
                .map_err(|_| anyhow::anyhow!("Invalid but resolving domain"))?,
            stream,
        )
        .await?;
    Ok(conn)
}

struct ControlConnection {
    addr: String,
    port: u16,
    local_port: u16,
    conn: tokio_rustls::client::TlsStream<tokio::net::TcpStream>,
    children: Vec<tokio::task::JoinHandle<anyhow::Result<()>>>,
}

impl ControlConnection {
    async fn connect(
        addr: String,
        port: u16,
        local_port: u16,
    ) -> anyhow::Result<ControlConnection> {
        let conn = tls_connect(addr.as_str(), port).await?;
        Ok(Self {
            addr,
            port,
            local_port,
            conn,
            children: vec![],
        })
    }

    async fn handle<T>(
        mut self,
        domain: Option<String>,
        on_domain_assigned: impl Fn(String) -> T,
    ) -> anyhow::Result<!>
    where
        T: std::future::Future<Output = anyhow::Result<()>>,
    {
        Message::RequestDomain(domain)
            .write_to(&mut self.conn)
            .await?;
        loop {
            let msg = Message::read_from(&mut self.conn).await?;
            match msg {
                Message::IncomingConnection(key) => {
                    self.children.push(tokio::task::spawn(connect_and_handle(
                        self.addr.clone(),
                        self.port,
                        self.local_port,
                        key,
                    )));
                }
                Message::AssignedDomain(domain) => {
                    on_domain_assigned(domain).await?;
                }
                _ => return Err(anyhow::anyhow!("Message recieved shouldn't be clientbound")),
            }
        }
    }
}

impl Drop for ControlConnection {
    fn drop(&mut self) {
        for child in self.children.drain(..) {
            child.abort();
        }
    }
}

async fn connect_and_handle(
    addr: String,
    port: u16,
    local_port: u16,
    key: AuthKey,
) -> anyhow::Result<()> {
    let mut conn = tls_connect(addr.as_str(), port).await?;
    let mut local_conn = tokio::net::TcpStream::connect(std::net::SocketAddrV4::new(
        std::net::Ipv4Addr::new(127, 0, 0, 1),
        local_port,
    ))
    .await?;
    Message::Authenticate(key).write_to(&mut conn).await?;
    let msg = Message::read_from(&mut conn).await?;
    match msg {
        Message::SwitchToNetty => (),
        _ => return Err(anyhow::anyhow!("Message recieved shouldn't be clientbound")),
    }
    tokio::io::copy_bidirectional(&mut local_conn, &mut conn).await?;
    Ok(())
}
