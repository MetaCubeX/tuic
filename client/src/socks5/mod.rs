use self::protocol::{
    handshake::{self, Socks5AuthMethod, Socks5PasswordAuthStatus},
    Error as Socks5Error, HandshakePasswordRequest, HandshakePasswordResponse, HandshakeRequest,
    HandshakeResponse, Reply, Request, Response,
};
use crate::{
    connection::{ConnectionError as TuicConnectionError, ConnectionRequest},
    ClientError, Config,
};
use quinn::{RecvStream as QuinnRecvStream, SendStream as QuinnSendStream};
use std::{io::Error as IoError, net::SocketAddr, sync::Arc};
use thiserror::Error;
use tokio::{
    io,
    net::{TcpListener, TcpStream},
    sync::mpsc::Sender as MpscSender,
};

mod convert;
mod protocol;

pub struct Socks5Server {
    request_sender: Arc<MpscSender<ConnectionRequest>>,
    local_addr: SocketAddr,
    auth_method: Socks5AuthMethod,
}

impl Socks5Server {
    pub fn new(config: &Config, sender: MpscSender<ConnectionRequest>) -> Self {
        Self {
            request_sender: Arc::new(sender),
            local_addr: config.local_addr,
            auth_method: {
                if config.username.is_some() && config.password.is_some() {
                    Socks5AuthMethod::PASSWORD {
                        username: config.username.clone().expect("username is required"),
                        password: config.password.clone().expect("password is required"),
                    }
                } else {
                    Socks5AuthMethod::NONE
                }
            },
        }
    }

    pub async fn run(self) -> Result<(), ClientError> {
        let socks5_listener = TcpListener::bind(self.local_addr).await?;

        while let Ok((stream, _)) = socks5_listener.accept().await {
            let mut socks5_conn =
                Socks5Connection::new(stream, &self.request_sender, self.auth_method.clone());

            tokio::spawn(async move {
                if let Err(err) = socks5_conn.process().await {
                    log::warn!("{err}");
                }
            });
        }

        Ok(())
    }
}

struct Socks5Connection {
    stream: TcpStream,
    request_sender: Arc<MpscSender<ConnectionRequest>>,
    auth_method: Socks5AuthMethod,
}

impl Socks5Connection {
    fn new(
        stream: TcpStream,
        request_sender: &Arc<MpscSender<ConnectionRequest>>,
        auth_method: Socks5AuthMethod,
    ) -> Self {
        Self {
            stream,
            request_sender: Arc::clone(request_sender),
            auth_method,
        }
    }

    async fn process(&mut self) -> Result<(), Socks5ConnectionError> {
        if !self.handshake().await? {
            log::warn!("[local]SOCKS5 Authentication failed");
            return Ok(());
        }

        let socks5_req = Request::read_from(&mut self.stream).await?;

        log::info!("[local]{:?} {:?}", &socks5_req.command, &socks5_req.address);

        let (req, res_receiver) =
            ConnectionRequest::new(socks5_req.command.into(), socks5_req.address.into());

        if self.request_sender.send(req).await.is_ok() {
            match res_receiver.await {
                Ok(Ok((mut remote_send, mut remote_recv))) => {
                    let socks5_res =
                        Response::new(Reply::Succeeded, SocketAddr::from(([0, 0, 0, 0], 0)).into());
                    socks5_res.write_to(&mut self.stream).await?;

                    self.forward(&mut remote_send, &mut remote_recv).await;

                    return Ok(());
                }
                Ok(Err(err)) => {
                    let reply = match err {
                        TuicConnectionError::Tuic(err) => Socks5Error::from(err).as_reply(),
                        _ => Reply::GeneralFailure,
                    };

                    let socks5_res =
                        Response::new(reply, SocketAddr::from(([0, 0, 0, 0], 0)).into());
                    socks5_res.write_to(&mut self.stream).await?;

                    return Ok(());
                }
                _ => {}
            }
        }

        let socks5_res = Response::new(
            Reply::GeneralFailure,
            SocketAddr::from(([0, 0, 0, 0], 0)).into(),
        );
        socks5_res.write_to(&mut self.stream).await?;

        Err(Socks5ConnectionError::ConnectionManager)
    }

    async fn handshake(&mut self) -> Result<bool, Socks5Error> {
        match &self.auth_method {
            handshake::Socks5AuthMethod::NONE => {
                let hs_req = HandshakeRequest::read_from(&mut self.stream).await?;
                if hs_req.methods.contains(&self.auth_method.as_u8()) {
                    let hs_res = HandshakeResponse::new(self.auth_method.as_u8());
                    hs_res.write_to(&mut self.stream).await?;
                    Ok(true)
                } else {
                    let hs_res =
                        HandshakeResponse::new(handshake::Socks5AuthMethod::UNACCEPTABLE.as_u8());
                    hs_res.write_to(&mut self.stream).await?;
                    Ok(false)
                }
            }
            handshake::Socks5AuthMethod::PASSWORD { .. } => {
                let hs_req = HandshakeRequest::read_from(&mut self.stream).await?;
                if hs_req.methods.contains(&self.auth_method.as_u8()) {
                    let hs_res = HandshakeResponse::new(self.auth_method.as_u8());
                    hs_res.write_to(&mut self.stream).await?;
                } else {
                    let hs_res =
                        HandshakeResponse::new(handshake::Socks5AuthMethod::UNACCEPTABLE.as_u8());
                    hs_res.write_to(&mut self.stream).await?;
                    return Ok(false);
                }

                let hs_password_req = HandshakePasswordRequest::read_from(&mut self.stream).await?;
                if hs_password_req.authenticated(&self.auth_method) {
                    let hs_password_res =
                        HandshakePasswordResponse::new(Socks5PasswordAuthStatus::SUCCESS.as_u8());
                    hs_password_res.write_to(&mut self.stream).await?;
                    Ok(true)
                } else {
                    let hs_password_res =
                        HandshakePasswordResponse::new(Socks5PasswordAuthStatus::FAILED.as_u8());
                    hs_password_res.write_to(&mut self.stream).await?;
                    Ok(false)
                }
            }
            // handshake::Socks5AuthMethod::GSSAPI => {}
            // TODO: implement GSSAPI
            _ => Ok(false),
        }
    }

    async fn forward(
        &mut self,
        remote_send: &mut QuinnSendStream,
        remote_recv: &mut QuinnRecvStream,
    ) {
        let (mut local_recv, mut local_send) = self.stream.split();
        let remote_to_local = io::copy(remote_recv, &mut local_send);
        let local_to_remote = io::copy(&mut local_recv, remote_send);
        let _ = tokio::try_join!(remote_to_local, local_to_remote);
    }
}

#[derive(Debug, Error)]
pub enum Socks5ConnectionError {
    #[error(transparent)]
    Io(#[from] IoError),
    #[error(transparent)]
    Socks5(#[from] Socks5Error),
    #[error("Failed to communicate with the connection manager")]
    ConnectionManager,
}
