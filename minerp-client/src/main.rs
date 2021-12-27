#![feature(never_type)]

use iced::Application;
use lazy_static::lazy_static;
use minerp_common::*;
use tokio_rustls::rustls;

#[macro_use]
extern crate log;

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

mod callback_future {
    struct Shared<T>
    where
        T: Send + Sync + 'static,
    {
        value: Option<T>,
        waker: Option<std::task::Waker>,
    }

    struct WrappedFuture<T>
    where
        T: Send + Sync + 'static,
    {
        shared: std::sync::Arc<std::sync::Mutex<Shared<T>>>,
    }

    impl<T> std::future::Future for WrappedFuture<T>
    where
        T: Send + Sync + 'static,
    {
        type Output = T;

        fn poll(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Self::Output> {
            trace!("Polling Future");
            let shared = &mut self.shared.lock().unwrap();
            trace!("Polling Future");
            if let Some(value) = shared.value.take() {
                trace!("Value got!");
                std::task::Poll::Ready(value)
            } else {
                let waker = &mut shared.waker;
                trace!("No value :( sending waker");
                *waker = Some(cx.waker().clone());
                std::task::Poll::Pending
            }
        }
    }

    fn resolve<T>(shared: std::sync::Arc<std::sync::Mutex<Shared<T>>>) -> Box<dyn FnOnce(T) + Send>
    where
        T: Send + Sync + 'static,
    {
        Box::new(move |value| {
            trace!("Resolved value, waker: {:?}", &shared.lock().unwrap().waker);
            shared.lock().unwrap().value = Some(value);
            if let Some(waker) = &shared.lock().unwrap().waker {
                trace!("Waking Future");
                waker.wake_by_ref()
            }
        })
    }

    pub fn create<T>() -> (
        impl std::future::Future<Output = T>,
        Box<dyn FnOnce(T) + Send>,
    )
    where
        T: Send + Sync + 'static,
    {
        let shared = std::sync::Arc::new(std::sync::Mutex::new(Shared {
            value: None,
            waker: None,
        }));
        (
            WrappedFuture {
                shared: shared.clone(),
            },
            resolve(shared),
        )
    }
}

#[derive(Clone)]
struct State {
    server_addr: String,
    server_port: u16,
    client_port: u16,
    client_addr: String,
    finalized: bool,
}

#[derive(Debug, Clone)]
enum AppMessage {
    ServerAddrChange(String),
    ServerPortChange(u16),
    ClientAddrChange(String),
    ClientPortChange(u16),
    DomainAssignment(String),
    FinalizeConfig,
    StubMessage,
}

struct App {
    state: State,
    server_addr_state: iced::widget::text_input::State,
    server_port_state: iced::widget::text_input::State,
    client_addr_state: iced::widget::text_input::State,
    client_port_state: iced::widget::text_input::State,
    finalize_button_s: iced::widget::button::State,
}

impl iced::Application for App {
    type Executor = iced::executor::Default;

    type Message = AppMessage;

    type Flags = State;

    fn new(flags: Self::Flags) -> (Self, iced::Command<Self::Message>) {
        (
            Self {
                state: flags,
                server_addr_state: Default::default(),
                server_port_state: Default::default(),
                client_addr_state: Default::default(),
                client_port_state: Default::default(),
                finalize_button_s: Default::default(),
            },
            iced::Command::none(),
        )
    }

    fn title(&self) -> String {
        "Minecraft Proxy Thingy".to_string()
    }

    fn update(
        &mut self,
        message: Self::Message,
        _: &mut iced::Clipboard,
    ) -> iced::Command<Self::Message> {
        trace!("Message recieved: {:?}", message);
        if self.state.finalized {
            if let AppMessage::DomainAssignment(addr) = message {
                self.state.client_addr = addr
            }
            self.client_addr_state.unfocus();
            self.client_port_state.unfocus();
            self.server_addr_state.unfocus();
            self.server_port_state.unfocus();
            return iced::Command::none();
        };
        match message {
            AppMessage::ServerAddrChange(addr) => self.state.server_addr = addr,
            AppMessage::ServerPortChange(port) => self.state.server_port = port,
            AppMessage::ClientAddrChange(addr) => self.state.client_addr = addr,
            AppMessage::ClientPortChange(port) => self.state.client_port = port,
            AppMessage::DomainAssignment(addr) => self.state.client_addr = addr,
            AppMessage::FinalizeConfig => {
                self.state.finalized = true;
                let state = self.state.clone();
                let (future, resolve) = callback_future::create();
                let cmd = iced::Command::perform(
                    async move {
                        let conn = ControlConnection::connect(
                            state.server_addr.clone(),
                            state.server_port,
                            state.client_port,
                        )
                        .await
                        .unwrap()
                        .handle(
                            if state.client_addr.is_empty() {
                                None
                            } else {
                                Some(state.client_addr.clone())
                            },
                            resolve,
                        );
                        conn.await
                    },
                    |_| AppMessage::StubMessage,
                );
                return iced::Command::batch([
                    iced::Command::perform(future, AppMessage::DomainAssignment),
                    cmd,
                ]);
            }
            AppMessage::StubMessage => (),
        }
        trace!("No Command Ran");
        iced::Command::none()
    }

    fn view(&mut self) -> iced::Element<'_, Self::Message> {
        use iced::widget::*;
        let col = Column::new()
            .push(
                Row::new()
                    .push(TextInput::new(
                        &mut self.server_addr_state,
                        "Proxy server address",
                        self.state.server_addr.as_str(),
                        AppMessage::ServerAddrChange,
                    ))
                    .push(TextInput::new(
                        &mut self.server_port_state,
                        "Port",
                        self.state.server_port.to_string().as_str(),
                        |port| match port.parse() {
                            Ok(port) => AppMessage::ServerPortChange(port),
                            Err(_) => AppMessage::StubMessage,
                        },
                    )),
            )
            .push(
                Row::new()
                    .push(TextInput::new(
                        &mut self.client_addr_state,
                        "Choose randomly",
                        self.state.client_addr.as_str(),
                        AppMessage::ClientAddrChange,
                    ))
                    .push(Text::new(
                        ".".to_string() + self.state.server_addr.as_str() + " -> localhost:",
                    ))
                    .push(TextInput::new(
                        &mut self.client_port_state,
                        "Port",
                        self.state.client_port.to_string().as_str(),
                        |port| match port.parse() {
                            Ok(port) => AppMessage::ClientPortChange(port),
                            Err(_) => AppMessage::StubMessage,
                        },
                    )),
            );
        if !self.state.finalized {
            col.push(
                Button::new(&mut self.finalize_button_s, Text::new("Start Proxy"))
                    .on_press(AppMessage::FinalizeConfig),
            )
            .into()
        } else {
            col.into()
        }
    }
}

fn main() -> iced::Result {
    env_logger::init();
    App::run(iced::Settings::with_flags(State {
        server_addr: "mc.bs2k.me".to_owned(),
        server_port: 25566,
        client_port: 25565,
        client_addr: String::new(),
        finalized: false,
    }))
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

    async fn handle(
        mut self,
        domain: Option<String>,
        on_domain_assigned: impl FnOnce(String),
    ) -> anyhow::Result<!> {
        trace!("Handler Running");
        Message::RequestDomain(domain)
            .write_to(&mut self.conn)
            .await?;
        let msg = Message::read_from(&mut self.conn).await?;
        match msg {
            Message::AssignedDomain(domain) => {
                trace!("Domain assigned: {}", domain);
                on_domain_assigned(domain);
            }
            _ => return Err(anyhow::anyhow!("Message recieved shouldn't be clientbound")),
        }
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
