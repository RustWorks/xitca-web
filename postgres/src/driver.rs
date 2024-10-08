pub(crate) mod codec;
pub(crate) mod generic;

mod connect;

pub(crate) use generic::DriverTx;

#[cfg(feature = "tls")]
mod tls;

#[cfg(feature = "quic")]
pub(crate) mod quic;

use core::{
    future::{Future, IntoFuture},
    net::SocketAddr,
    pin::Pin,
};

use postgres_protocol::message::backend;
use xitca_io::{
    io::{AsyncIo, AsyncIoDyn},
    net::TcpStream,
};

use super::{client::Client, config::Config, error::Error, iter::AsyncLendingIterator};

use self::generic::GenericDriver;

#[cfg(feature = "tls")]
use xitca_tls::rustls::{ClientConnection, TlsStream};

#[cfg(unix)]
use xitca_io::net::UnixStream;

pub(super) async fn connect(cfg: &mut Config) -> Result<(Client, Driver), Error> {
    let mut err = None;
    let hosts = cfg.get_hosts().to_vec();
    for host in hosts {
        match self::connect::connect_host(host, cfg).await {
            Ok((tx, drv)) => return Ok((Client::new(tx), drv)),
            Err(e) => err = Some(e),
        }
    }

    Err(err.unwrap())
}

pub(super) async fn connect_io<Io>(io: Io, cfg: &mut Config) -> Result<(Client, Driver), Error>
where
    Io: AsyncIo + Send + 'static,
{
    let (tx, drv) = self::connect::connect_io(io, cfg).await?;
    Ok((Client::new(tx), drv))
}

async fn dns_resolve(host: &str, ports: &[u16]) -> Result<Vec<SocketAddr>, Error> {
    let addrs = tokio::net::lookup_host((host, 0))
        .await?
        .flat_map(|mut addr| {
            ports.iter().map(move |port| {
                addr.set_port(*port);
                addr
            })
        })
        .collect::<Vec<_>>();
    Ok(addrs)
}

/// async driver of [Client](crate::Client).
/// it handles IO and emit server sent message that do not belong to any query with [AsyncLendingIterator]
/// trait impl.
///
/// # Examples:
/// ```rust
/// use std::future::IntoFuture;
/// use xitca_postgres::{AsyncLendingIterator, Driver};
///
/// // drive the client and listen to server notify at the same time.
/// fn drive_with_server_notify(mut drv: Driver) {
///     tokio::spawn(async move {
///         while let Ok(Some(msg)) = drv.try_next().await {
///             // *Note:
///             // handle message must be non-blocking to prevent starvation of driver.
///         }
///     });
/// }
///
/// // drive client without handling notify.
/// fn drive_only(drv: Driver) {
///     tokio::spawn(drv.into_future());
/// }
/// ```
// TODO: use Box<dyn AsyncIterator> when life time GAT is object safe.
pub enum Driver {
    Tcp(GenericDriver<TcpStream>),
    Dynamic(GenericDriver<Box<dyn AsyncIoDyn + Send>>),
    #[cfg(feature = "tls")]
    Tls(GenericDriver<TlsStream<ClientConnection, TcpStream>>),
    #[cfg(unix)]
    Unix(GenericDriver<UnixStream>),
    #[cfg(all(unix, feature = "tls"))]
    UnixTls(GenericDriver<TlsStream<ClientConnection, UnixStream>>),
    #[cfg(feature = "quic")]
    Quic(GenericDriver<crate::driver::quic::QuicStream>),
}

impl AsyncLendingIterator for Driver {
    type Ok<'i> = backend::Message where Self: 'i;
    type Err = Error;

    #[inline]
    async fn try_next(&mut self) -> Result<Option<Self::Ok<'_>>, Self::Err> {
        match self {
            Self::Tcp(ref mut drv) => drv.try_next().await,
            Self::Dynamic(ref mut drv) => drv.try_next().await,
            #[cfg(feature = "tls")]
            Self::Tls(ref mut drv) => drv.try_next().await,
            #[cfg(unix)]
            Self::Unix(ref mut drv) => drv.try_next().await,
            #[cfg(all(unix, feature = "tls"))]
            Self::UnixTls(ref mut drv) => drv.try_next().await,
            #[cfg(feature = "quic")]
            Self::Quic(ref mut drv) => drv.try_next().await,
        }
    }
}

impl IntoFuture for Driver {
    type Output = ();
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + Send>>;

    fn into_future(mut self) -> Self::IntoFuture {
        Box::pin(async move { while self.try_next().await.ok().flatten().is_some() {} })
    }
}
