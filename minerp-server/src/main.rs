#![feature(never_type)]
#![feature(try_blocks)]
#![feature(result_flattening)]

use lazy_static::lazy_static;
use minerp_common::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::rustls;
use warp::Filter;

#[macro_use]
extern crate log;

mod netty {
    use anyhow::Result;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    pub async fn read_bigendian_u16<T: tokio::io::AsyncRead + Unpin>(buf: &mut T) -> Result<u16> {
        Ok(((buf.read_u8().await? as u16) << 8) + buf.read_u8().await? as u16)
    }

    pub async fn write_bigendian_u16<T: tokio::io::AsyncWrite + Unpin>(
        buf: &mut T,
        n: u16,
    ) -> Result<()> {
        buf.write_u8((n >> 8) as u8).await?;
        buf.write_u8((n & 0xFF) as u8).await?;
        Ok(())
    }

    pub async fn read_varint<T: tokio::io::AsyncRead + Unpin>(buf: &mut T) -> Result<i32> {
        let mut res = 0i32;
        for i in 0..5 {
            let part = buf.read_u8().await?;
            res |= (part as i32 & 0x7F) << (7 * i);
            if part & 0x80 == 0 {
                return Ok(res);
            }
        }
        Err(anyhow::anyhow!("VarInt too big!"))
    }

    pub async fn read_string<T: tokio::io::AsyncRead + Unpin>(buf: &mut T) -> Result<String> {
        let len = read_varint(buf).await? as usize;
        let mut strbuf = vec![0; len as usize];
        buf.read_exact(&mut strbuf).await?;
        Ok(String::from_utf8(strbuf)?)
    }

    pub async fn write_varint<T: tokio::io::AsyncWrite + Unpin>(
        buf: &mut T,
        mut val: i32,
    ) -> Result<()> {
        for _ in 0..5 {
            if val & !0x7F == 0 {
                buf.write_u8(val as u8).await?;
                return Ok(());
            }
            buf.write_u8((val & 0x7F | 0x80) as u8).await?;
            val >>= 7;
        }
        Err(anyhow::anyhow!("VarInt too big!"))
    }

    pub async fn write_string<T: tokio::io::AsyncWrite + Unpin>(
        buf: &mut T,
        s: &str,
    ) -> Result<()> {
        write_varint(buf, s.len() as i32).await?;
        buf.write_all(s.as_bytes()).await?;
        Ok(())
    }
}

lazy_static! {
    static ref CONN_CONF: std::sync::Arc<rustls::ServerConfig> = {
        let chain = std::env::var("CERTCHAIN").expect("CERTCHAIN not set");
        let chain = std::fs::File::open(chain).expect("Could not open certificate chain");
        let mut chain = std::io::BufReader::new(chain);
        let chain = rustls_pemfile::certs(&mut chain).expect("Could not parse certificate chain");
        let chain = chain
            .into_iter()
            .map(rustls::Certificate)
            .collect::<Vec<_>>();
        let key = std::env::var("PRIVKEY").expect("PRIVKEY not set");
        let key = std::fs::File::open(key).expect("Could not open private key");
        let mut key = std::io::BufReader::new(key);
        let key =
            rustls_pemfile::pkcs8_private_keys(&mut key).expect("Could not parse private key");
        let key = rustls::PrivateKey(key.get(0).unwrap().clone());
        let conf = rustls::ServerConfig::builder()
            .with_safe_defaults()
            .with_no_client_auth()
            .with_single_cert(chain, key)
            .expect("Invalid private key");
        std::sync::Arc::new(conf)
    };
    static ref ROUTER_MINECRAFT: std::sync::Arc<Router<String, MinecraftConnection>> =
        std::sync::Arc::new(Router::new());
    static ref ROUTER_PROXY: std::sync::Arc<Router<AuthKey, tokio_rustls::server::TlsStream<tokio::net::TcpStream>>> =
        std::sync::Arc::new(Router::new());
    static ref LISTEN_ADDR: std::sync::Arc<str> = {
        let addr = std::env::var("LISTEN_ADDR").expect("LISTEN_ADDR not set");
        std::sync::Arc::from(addr.as_str())
    };
    static ref API_TASKS: std::sync::Arc<
        tokio::sync::Mutex<std::collections::HashMap<String, tokio::task::JoinHandle<!>>>,
    > = std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
}

#[tokio::main]
async fn main() {
    env_logger::init();
    let _ = CONN_CONF.clone(); // eager evaluate lazy static
    let minecraft_handle = tokio::task::spawn(accept(25565, handle_minecraft));
    let proxy_handle = tokio::task::spawn(accept(25566, handle_proxy));
    let api_handle = tokio::task::spawn(serve_api());
    let minecraft_err = minecraft_handle
        .await
        .map_err::<anyhow::Error, _>(|e| e.into())
        .flatten();
    match minecraft_err {
        Ok(_) => (),
        Err(e) => error!("Minecraft handler error: {}", e),
    }
    let proxy_err = proxy_handle
        .await
        .map_err::<anyhow::Error, _>(|e| e.into())
        .flatten();
    match proxy_err {
        Ok(_) => (),
        Err(e) => error!("Proxy handler error: {}", e),
    }
    match api_handle.await {
        Ok(()) => (),
        Err(e) => error!("API handler error: {}", e),
    }
}

async fn serve_api() {
    let register =
        warp::path!("register" / String / String).then(|domain: String, dest: String| async move {
            let cloned_domain = domain.clone();
            let mut result = r#"{"success":false}"#;
            if ROUTER_MINECRAFT.clone().exists(&domain).unwrap_or(true) {
                result = r#"{"success":false}"#
            }
            let task = tokio::task::spawn(async move {
                let mut guard = RouteGuard::new(ROUTER_MINECRAFT.clone(), cloned_domain).unwrap();
                loop {
                    let result: anyhow::Result<()> = try {
                        let mut incoming_conn = guard.wait().await;
                        let mut proxy_conn = tokio::net::TcpStream::connect(dest.as_str()).await?;
                        tokio::task::spawn(async move {
                            let result: anyhow::Result<()> = try {
                                use netty::*;
                                let mut handshake_buf = vec![];
                                write_varint(&mut handshake_buf, 0_i32).await?;
                                write_varint(&mut handshake_buf, incoming_conn.protocol_version)
                                    .await?;
                                write_string(&mut handshake_buf, "minerp.proxied.connection")
                                    .await?;
                                write_bigendian_u16(&mut handshake_buf, 25565).await?;
                                write_varint(
                                    &mut handshake_buf,
                                    if incoming_conn.is_serverlistping {
                                        1
                                    } else {
                                        2
                                    },
                                )
                                .await?;
                                write_varint(&mut proxy_conn, handshake_buf.len().try_into()?)
                                    .await?;
                                proxy_conn.write_all(handshake_buf.as_slice()).await?;
                                tokio::io::copy_bidirectional(
                                    &mut incoming_conn.conn,
                                    &mut proxy_conn,
                                )
                                .await?;
                            };
                            if let Err(e) = result {
                                warn!("{}", e);
                            }
                        });
                    };
                    if let Err(e) = result {
                        warn!("{}", e);
                    };
                }
            });
            API_TASKS.lock().await.insert(domain, task);
            result
        });
    let unregister = warp::path!("unregister" / String).then(|domain: String| async move {
        if let Some(task) = API_TASKS.lock().await.remove(&domain) {
            task.abort();
            r#"{"success":true}"#
        } else {
            r#"{"success":false}"#
        }
    });
    let list = warp::path!("list").then(|| async move {
        let result: anyhow::Result<String> = try {
            let table = ROUTER_MINECRAFT
                .table
                .read()
                .map_err(|e| anyhow::anyhow!("Poisoned RWLock: {:?}", e))?;
            let domains = table
                .keys()
                .collect::<Vec<_>>();
            serde_json::json!({
                "success": true,
                "result": domains
            }).to_string()
        };
        match result {
            Ok(result) => result,
            Err(e) => {
                warn!("{}", e);
                r#"{"success":false}"#.to_owned()
            }
        }
    });
    warp::serve(register.or(unregister).or(list))
        .run(([0, 0, 0, 0], 25567))
        .await;
}

#[derive(Debug)]
struct MinecraftConnection {
    conn: tokio::net::TcpStream,
    protocol_version: i32,
    is_serverlistping: bool,
}

fn random_id() -> String {
    format!("{:08X}", rand::random::<u32>())
}

async fn accept<T>(port: u16, handler: impl Fn(tokio::net::TcpStream) -> T) -> anyhow::Result<!>
where
    T: std::future::Future<Output = ()> + std::marker::Send + 'static,
{
    let addr = std::net::SocketAddrV4::new(std::net::Ipv4Addr::new(0, 0, 0, 0), port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    loop {
        let (conn, addr) = match listener.accept().await {
            Ok(conn) => conn,
            Err(_) => {
                error!("Failed to accept connection");
                continue;
            }
        };
        debug!("Accepted connection from {}", addr);
        tokio::task::spawn(handler(conn));
    }
}

async fn handle_proxy(conn: tokio::net::TcpStream) {
    let result: anyhow::Result<()> = try {
        let mut conn = tokio_rustls::TlsAcceptor::from(CONN_CONF.clone())
            .accept(conn)
            .await?;
        let msg = Message::read_from(&mut conn).await?;
        let mut domain = String::new();
        let router_minecraft = ROUTER_MINECRAFT.clone();
        let router_proxy = ROUTER_PROXY.clone();
        match msg {
            Message::RequestDomain(requested_domain) => {
                domain = match requested_domain {
                    Some(domain) => domain,
                    None => random_id(),
                };
            }
            Message::Authenticate(authcode) => {
                router_proxy.route(&authcode, conn)?;
                return;
            }
            _ => {
                Err(anyhow::anyhow!("Message recieved shouldn't be serverbound"))?;
            }
        }
        while router_minecraft.exists(&domain).unwrap() {
            domain = random_id();
        }
        domain = domain.to_lowercase();
        Message::AssignedDomain(domain.clone())
            .write_to(&mut conn)
            .await?;
        let mut guard_minecraft = RouteGuard::new(router_minecraft, domain).unwrap();
        loop {
            let mut incoming_conn = guard_minecraft.wait().await;
            let authkey = rand::random();
            Message::IncomingConnection(authkey)
                .write_to(&mut conn)
                .await?;
            let mut conn_guard = RouteGuard::new(router_proxy.clone(), authkey).unwrap();
            tokio::task::spawn(async move {
                let result: anyhow::Result<()> = try {
                    use netty::*;
                    let mut proxy_conn = conn_guard.wait().await;
                    Message::SwitchToNetty.write_to(&mut proxy_conn).await?;
                    let mut handshake_buf = vec![];
                    write_varint(&mut handshake_buf, 0_i32).await?;
                    write_varint(&mut handshake_buf, incoming_conn.protocol_version).await?;
                    write_string(&mut handshake_buf, "minerp.proxied.connection").await?;
                    write_bigendian_u16(&mut handshake_buf, 25565).await?;
                    write_varint(
                        &mut handshake_buf,
                        if incoming_conn.is_serverlistping {
                            1
                        } else {
                            2
                        },
                    )
                    .await?;
                    write_varint(&mut proxy_conn, handshake_buf.len().try_into()?).await?;
                    proxy_conn.write_all(handshake_buf.as_slice()).await?;
                    tokio::io::copy_bidirectional(&mut incoming_conn.conn, &mut proxy_conn).await?;
                };
                if let Err(e) = result {
                    warn!("{}", e);
                }
            });
        }
    };
    if let Err(err) = result {
        warn!("Error handling connection from proxy: {:?}", err);
    };
}

async fn handle_minecraft(mut conn: tokio::net::TcpStream) {
    let result: anyhow::Result<()> = try {
        use netty::*;
        let len = read_varint(&mut conn).await?;
        debug!("Read handshake length: {}", len);
        if len == 254 {
            debug!("Legacy ServerListPing response");
            // && id == 122 doesn't catch <= 1.5
            // Protocol 127
            // Version 2.0.0
            // MOTD Minecraft versions below1.6isn't supported
            // TODO: fix typo
            conn.write_all(&[
                0xff, 0x00, 0x3b, 0x00, 0xa7, 0x00, 0x31, 0x00, 0x00, 0x00, 0x31, 0x00, 0x32, 0x00,
                0x37, 0x00, 0x00, 0x00, 0x32, 0x00, 0x2e, 0x00, 0x30, 0x00, 0x2e, 0x00, 0x30, 0x00,
                0x00, 0x00, 0x4d, 0x00, 0x69, 0x00, 0x6e, 0x00, 0x65, 0x00, 0x63, 0x00, 0x72, 0x00,
                0x61, 0x00, 0x66, 0x00, 0x74, 0x00, 0x20, 0x00, 0x76, 0x00, 0x65, 0x00, 0x72, 0x00,
                0x73, 0x00, 0x69, 0x00, 0x6f, 0x00, 0x6e, 0x00, 0x73, 0x00, 0x20, 0x00, 0x62, 0x00,
                0x65, 0x00, 0x6c, 0x00, 0x6f, 0x00, 0x77, 0x00, 0x31, 0x00, 0x2e, 0x00, 0x36, 0x00,
                0x69, 0x00, 0x73, 0x00, 0x6e, 0x00, 0x27, 0x00, 0x74, 0x00, 0x20, 0x00, 0x73, 0x00,
                0x75, 0x00, 0x70, 0x00, 0x70, 0x00, 0x6f, 0x00, 0x72, 0x00, 0x74, 0x00, 0x65, 0x00,
                0x64, 0x00, 0x00, 0x00, 0x30, 0x00, 0x00, 0x00, 0x30,
            ])
            .await?;
            return;
        }
        let id = read_varint(&mut conn).await?;

        if id != 0 {
            Err(anyhow::anyhow!("Invalid packet id"))?;
        }

        let protocol_version = read_varint(&mut conn).await?;
        let addr = read_string(&mut conn).await?;
        // Incorrect on 13w41a but no way to tell
        let _port = read_bigendian_u16(&mut conn).await?; // don't need it
        let is_serverlistping = match read_varint(&mut conn).await? {
            // actually varint but indistinguishible from a byte because it's either 1 or 2
            1 => true,
            2 => false,
            _ => Err(anyhow::anyhow!(
                "client sent invalid data for desired state"
            ))?,
        };

        let addr = addr
            .strip_suffix(&(".".to_string() + LISTEN_ADDR.clone().to_string().as_str()))
            .ok_or_else(|| anyhow::anyhow!("Invalid hostname to call from"))?;
        if ROUTER_MINECRAFT.exists(&addr.to_owned()).unwrap() {
            ROUTER_MINECRAFT.route(
                &addr.to_owned(),
                MinecraftConnection {
                    conn,
                    protocol_version,
                    is_serverlistping,
                },
            )?;
        } else if is_serverlistping {
            let len = read_varint(&mut conn).await?;
            if len != 1 {
                Err(anyhow::anyhow!("Invalid serverlistping request"))?;
            }
            let request = read_varint(&mut conn).await?;
            if request != 0 {
                Err(anyhow::anyhow!("Invalid serverlistping request"))?;
            }
            let mut serverlistping_buf = vec![];
            write_varint(&mut serverlistping_buf, 0).await?;
            write_string(&mut serverlistping_buf, "{\"version\":{\"name\":\"No Server Here\",\"protocol\":2147483647},\"players\":{\"max\":0,\"online\":0},\"description\":{\"text\":\"This address isn't proxied to any server. Check your address.\"},\"favicon\":\"data:image/png;base64,<data>\"}").await?;
            write_varint(&mut conn, serverlistping_buf.len().try_into()?).await?;
            conn.write_all(serverlistping_buf.as_slice()).await?;
            let mut ping = [0u8; 10];
            conn.read_exact(&mut ping).await?;
            conn.write_all(&ping).await?;
        } else {
            let mut kick_buf = vec![];
            write_varint(&mut kick_buf, 0).await?;
            write_string(
                &mut kick_buf,
                "{\"text\":\"This address isn't proxied to any server. Check your address.\"}",
            )
            .await?;
            write_varint(&mut conn, kick_buf.len().try_into()?).await?;
            conn.write_all(kick_buf.as_slice()).await?;
        }
    };
    if let Err(err) = result {
        warn!("Error handling connection from minecraft: {:?}", err)
    };
}

#[derive(Debug)]
struct Router<T, U>
where
    T: Clone + std::hash::Hash + std::cmp::Eq + std::fmt::Debug,
    U: 'static + std::marker::Sync + std::marker::Send + std::fmt::Debug,
{
    table: std::sync::RwLock<std::collections::HashMap<T, tokio::sync::mpsc::UnboundedSender<U>>>,
}

impl<T, U> Router<T, U>
where
    T: Clone + std::hash::Hash + std::cmp::Eq + std::fmt::Debug,
    U: 'static + std::marker::Sync + std::marker::Send + std::fmt::Debug,
{
    fn new() -> Self {
        Self {
            table: std::sync::RwLock::new(std::collections::HashMap::new()),
        }
    }

    fn exists(&self, name: &T) -> Option<bool> {
        Some(self.table.read().ok()?.contains_key(name))
    }

    fn register(
        &self,
        name: T,
        queue: tokio::sync::mpsc::UnboundedSender<U>,
    ) -> anyhow::Result<()> {
        self.table
            .write()
            .map_err(|_| anyhow::anyhow!("HashMap was poisoned"))?
            .insert(name, queue);
        Ok(())
    }

    fn unregister(&self, name: &T) -> anyhow::Result<()> {
        self.table
            .write()
            .map_err(|_| anyhow::anyhow!("HashMap was poisoned"))?
            .remove(name);
        Ok(())
    }

    fn route(&self, name: &T, conn: U) -> anyhow::Result<()> {
        self.table
            .read()
            .map_err(|_| anyhow::anyhow!("HashMap was poisoned"))?
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("No such connection"))?
            .send(conn)?;
        Ok(())
    }
}

struct RouteGuard<T, U>
where
    T: Clone + std::hash::Hash + std::cmp::Eq + std::fmt::Debug,
    U: 'static + std::marker::Sync + std::marker::Send + std::fmt::Debug,
{
    router: std::sync::Arc<Router<T, U>>,
    name: T,
    queue: tokio::sync::mpsc::UnboundedReceiver<U>,
}

impl<T, U> Drop for RouteGuard<T, U>
where
    T: Clone + std::hash::Hash + std::cmp::Eq + std::fmt::Debug,
    U: 'static + std::marker::Sync + std::marker::Send + std::fmt::Debug,
{
    fn drop(&mut self) {
        self.router.unregister(&self.name).unwrap(); // HashMap's poisoned anyways it's already so dead
    }
}

impl<T, U> RouteGuard<T, U>
where
    T: Clone + std::hash::Hash + std::cmp::Eq + std::fmt::Debug,
    U: 'static + std::marker::Sync + std::marker::Send + std::fmt::Debug,
{
    fn new(router: std::sync::Arc<Router<T, U>>, name: T) -> Option<Self> {
        let (send, recv) = tokio::sync::mpsc::unbounded_channel();
        router.register(name.clone(), send).ok()?;
        Some(Self {
            router,
            name,
            queue: recv,
        })
    }

    async fn wait(&mut self) -> U {
        self.queue.recv().await.unwrap() // Can never panic under normal circumstances
    }
}
