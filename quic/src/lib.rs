use quiche;
use std::io;
use std::net::SocketAddr;
use std::net::UdpSocket as StdUdpSocket;
use tokio;

pub struct QuicSocket {
    pub inner: tokio::net::UdpSocket,
    pub addr: SocketAddr,
    pub bind_addr: Option<SocketAddr>,
}

pub struct QuicListener<'a> {
    socket: &'a QuicSocket,
    connection: std::pin::Pin<std::boxed::Box<quiche::Connection>>,
}

impl QuicSocket {
    pub async fn bind(addr: SocketAddr) -> io::Result<QuicSocket> {
        match tokio::net::UdpSocket::bind(addr).await {
            Ok(inner) => Ok(QuicSocket {
                inner,
                addr: addr,
                bind_addr: None,
            }),
            Err(_) => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "could not resolve to any address",
            )),
        }
    }

    pub fn from_std(socket: StdUdpSocket) -> io::Result<QuicSocket> {
        let inner = match tokio::net::UdpSocket::from_std(socket) {
            Ok(inner) => inner,
            Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "could not resolve from std socket",
                ))
            }
        };
        let addr = match inner.local_addr() {
            Ok(addr) => addr,
            Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "could not resolve to local address",
                ))
            }
        };
        Ok(QuicSocket {
            inner,
            addr: addr,
            bind_addr: None,
        })
    }

    pub fn accept(&self, addr: SocketAddr, mut config: quiche::Config) -> io::Result<QuicListener> {
        if self.bind_addr.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "underlying socket not bound",
            ));
        };
        let scid = quiche::ConnectionId::from_ref(&[0xba; 16]);
        let connection = match quiche::accept(&scid, None, addr, &mut config) {
            Ok(conn) => conn,
            Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    "could not connect",
                ))
            }
        };
        Ok(QuicListener {
            socket: self,
            connection,
        })
    }

    pub fn connect(&self, addr: SocketAddr, mut config: quiche::Config) -> io::Result<QuicListener> {
        if self.bind_addr.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "underlying socket not bound",
            ));
        };
        let scid = quiche::ConnectionId::from_ref(&[0xba; 16]);
        let connection = match quiche::connect(None, &scid, addr, &mut config) {
            Ok(conn) => conn,
            Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    "could not connect",
                ))
            }
        };
        Ok(QuicListener {
            socket: self,
            connection,
        })
    }
}
