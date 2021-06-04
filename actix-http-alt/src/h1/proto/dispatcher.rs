use std::{
    future::Future,
    io,
    marker::PhantomData,
    pin::Pin,
    task::{self, Poll},
};

use actix_service_alt::Service;
use bytes::{Buf, Bytes};
use futures_core::stream::Stream;
use http::{response::Parts, Request, Response};
use log::trace;
use pin_project_lite::pin_project;
use tokio::{io::Interest, select};

use crate::body::ResponseBody;
use crate::error::BodyError;
use crate::flow::HttpFlow;
use crate::h1::{
    body::{RequestBody, RequestBodySender},
    error::Error,
};
use crate::response::ResponseError;
use crate::stream::AsyncStream;
use crate::util::{date::DateTimeTask, poll_fn::poll_fn};

use super::buf::{ReadBuf, WriteBuf};
use super::context::{ConnectionType, Context};
use super::decode::{RequestBodyDecoder, RequestBodyItem};
use super::encode::TransferEncoding;
use super::keep_alive::KeepAlive;
use super::state::State;

pub(crate) struct Dispatcher<'a, St, S, B, X, U> {
    io: Io<'a, St>,
    timer: Pin<&'a mut KeepAlive>,
    ctx: Context<'a>,
    error: Option<Error>,
    flow: &'a HttpFlow<S, X, U>,
    _phantom: PhantomData<B>,
}

struct Io<'a, St> {
    io: &'a mut St,
    state: State,
    read_buf: ReadBuf,
    write_buf: WriteBuf,
}

impl<St> Io<'_, St>
where
    St: AsyncStream,
{
    /// read until blocked and advance readbuf.
    fn try_read(&mut self) -> Result<(), Error> {
        let read_buf = &mut self.read_buf;
        read_buf.advance(false);

        loop {
            match self.io.try_read_buf(read_buf.buf_mut()) {
                Ok(0) => return Err(Error::Closed),
                Ok(_) => read_buf.advance(true),
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(ref e) if e.kind() == io::ErrorKind::ConnectionReset => {
                    return Err(Error::Closed);
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Return true when write is blocked and need wait.
    /// Return false when write is finished.(Did not blocked)
    fn try_write(&mut self) -> Result<bool, Error> {
        match self.write_buf {
            WriteBuf::List(ref mut list) => {
                let queue = list.list_mut();
                while queue.remaining() > 0 {
                    let mut iovs = [io::IoSlice::new(&[]); 64];
                    let len = queue.chunks_vectored(&mut iovs);
                    match self.io.try_write_vectored(&iovs[..len]) {
                        Ok(0) => return Err(Error::Closed),
                        Ok(n) => {
                            queue.advance(n);
                        }
                        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                            return Ok(true);
                        }
                        Err(e) => return Err(e.into()),
                    }
                }
            }
            WriteBuf::Flat(ref mut buf) => {
                let mut written = 0;
                let len = buf.len();

                while written < len {
                    match self.io.try_write(&buf[written..]) {
                        Ok(0) => return Err(Error::Closed),
                        Ok(n) => written += n,
                        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                            buf.advance(written);
                            return Ok(true);
                        }
                        Err(e) => return Err(e.into()),
                    }
                }

                buf.clear();
            }
        }

        Ok(false)
    }

    /// Block task and read.
    #[inline(always)]
    async fn read(&mut self) -> Result<(), Error> {
        let _ = self.io.ready(Interest::READABLE).await?;
        self.try_read()
    }

    /// drain write buffer and flush the io.
    #[inline(always)]
    async fn drain_write(&mut self) -> Result<(), Error> {
        while self.try_write()? {
            let _ = self.io.ready(Interest::WRITABLE).await?;
        }
        poll_fn(|cx| Pin::new(&mut *self.io).poll_flush(cx)).await?;
        Ok(())
    }

    /// Return true when new data is decoded.
    /// Return false when write is finished.(Did not blocked)
    fn poll_read_decode_body(
        &mut self,
        body_handle: &mut Option<RequestBodyHandle>,
        cx: &mut task::Context<'_>,
    ) -> Result<bool, Error> {
        match *body_handle {
            Some(ref mut handle) => {
                let mut new = false;
                let mut done = false;

                // TODO: read error here should be treated as partial close.
                // Which means body_handle should treat error as finished read.
                // pass the partial buffer to service call and let it decide what to do.
                'read: while self.io.poll_read_ready(cx)?.is_ready() {
                    let _ = self.try_read()?;

                    let buf = &mut self.read_buf;

                    if buf.advanced() {
                        while let Some(item) = handle.decoder.decode(buf.buf_mut())? {
                            new = true;
                            match item {
                                RequestBodyItem::Chunk(bytes) => handle.sender.feed_data(bytes),
                                RequestBodyItem::Eof => {
                                    handle.sender.feed_eof();
                                    done = true;
                                    break 'read;
                                }
                            }
                        }
                    }
                }

                // remove body handle when client sent eof chunk.
                // No more read is needed.
                if done {
                    *body_handle = None;
                }

                Ok(new)
            }
            None => Ok(false),
        }
    }

    #[inline(always)]
    fn write_buf_mut(&mut self) -> &mut WriteBuf {
        &mut self.write_buf
    }
}

impl<'a, St, S, ResB, E, X, U> Dispatcher<'a, St, S, ResB, X, U>
where
    S: Service<Request<RequestBody>, Response = Response<ResponseBody<ResB>>> + 'static,
    S::Error: ResponseError<S::Response>,

    X: Service<Request<RequestBody>, Response = Request<RequestBody>> + 'static,
    X::Error: ResponseError<S::Response>,

    ResB: Stream<Item = Result<Bytes, E>>,
    BodyError: From<E>,

    St: AsyncStream,
{
    pub(crate) fn new(
        io: &'a mut St,
        timer: Pin<&'a mut KeepAlive>,
        flow: &'a HttpFlow<S, X, U>,
        date: &'a DateTimeTask,
    ) -> Self {
        let is_vectored = io.is_write_vectored();

        let io = Io {
            io,
            state: State::new(),
            read_buf: ReadBuf::new(),
            write_buf: WriteBuf::new(false),
        };

        Self {
            io,
            timer,
            ctx: Context::new(date.get()),
            error: None,
            flow,
            _phantom: PhantomData,
        }
    }

    fn decode_head(&mut self) -> Result<Option<DecodedHead>, Error> {
        // Do not try when nothing new read.
        if self.io.read_buf.advanced() {
            let buf = self.io.read_buf.buf_mut();

            if let Some((req, decoder)) = self.ctx.decode_head(buf)? {
                let (body_handle, body) = RequestBodyHandle::new_pair(decoder);

                let (parts, _) = req.into_parts();
                let req = Request::from_parts(parts, body);

                return Ok(Some((req, body_handle)));
            }
        }

        Ok(None)
    }

    fn encode_head(&mut self, parts: Parts, body: &ResponseBody<ResB>) -> Result<(), Error> {
        let size = body.size();
        let write_buf = self.io.write_buf_mut();
        self.ctx.encode_head(parts, size, write_buf)?;
        Ok(())
    }

    pub(crate) async fn run(&mut self) -> Result<(), Error> {
        loop {
            while let Some((req, mut body_handle)) = self.decode_head()? {
                // have new connection. update timer deadline.
                let now = self.ctx.date.get().now();
                self.timer.as_mut().update(now);

                let res = self.request_handler(req, &mut body_handle).await?;

                let (parts, res_body) = res.into_parts();

                self.encode_head(parts, &res_body)?;

                let encoder = res_body.encoder(self.ctx.ctype());

                ResponseHandler {
                    res_body,
                    encoder,
                    body_handle,
                    io: &mut self.io,
                }
                .await?
            }

            self.io.drain_write().await?;

            match self.ctx.ctype() {
                ConnectionType::Init => {
                    // use timer to detect slow connection.
                    select! {
                        res = self.io.read() => res?,
                        _ = self.timer.as_mut() => {
                            trace!("Slow Connection detected. Shutting down");
                            return Ok(())
                        }
                    }
                }
                ConnectionType::KeepAlive => {
                    select! {
                        res = self.io.read() => res?,
                        _ = self.timer.as_mut() => {
                            if self.timer.as_mut().is_expired() {
                                trace!("Connection keep-alive timeout. Shutting down");
                                return Ok(());
                            } else {
                                self.timer.as_mut().reset();
                            }
                        }
                    }
                }
                ConnectionType::Upgrade | ConnectionType::Close => {
                    trace!("Connection not keep-alive. Shutting down");
                    return Ok(());
                }
            }
        }
    }

    async fn request_handler(
        &mut self,
        mut req: Request<RequestBody>,
        body_handle: &mut Option<RequestBodyHandle>,
    ) -> Result<Response<ResponseBody<ResB>>, Error> {
        if self.ctx.is_expect() {
            match self.flow.expect.call(req).await {
                Ok(expect_res) => {
                    // encode continue
                    self.ctx.encode_continue(self.io.write_buf_mut());

                    // use drain write to make sure continue is sent to client.
                    // the world can wait until it happens.
                    self.io.drain_write().await?;

                    req = expect_res;
                }
                Err(e) => return Ok(ResponseError::response_error(e)),
            }
        };

        RequestHandler {
            fut: self.flow.service.call(req),
            body_handle,
            io: &mut self.io,
            _body: PhantomData,
        }
        .await
    }

    fn set_read_close(&mut self) {
        self.io.state.set_read_close();
    }

    fn set_write_close(&mut self) {
        self.io.state.set_write_close();
    }
}

pin_project! {
    struct RequestHandler<'a, 'b, St, Fut, ResB> {
        #[pin]
        fut: Fut,
        body_handle: &'a mut Option<RequestBodyHandle>,
        io: &'a mut Io<'b, St>,
        _body: PhantomData<ResB>
    }
}

impl<St, Fut, E, ResB> Future for RequestHandler<'_, '_, St, Fut, ResB>
where
    Fut: Future<Output = Result<Response<ResponseBody<ResB>>, E>>,
    E: ResponseError<Response<ResponseBody<ResB>>>,

    St: AsyncStream,
{
    type Output = Result<Response<ResponseBody<ResB>>, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();

        loop {
            match this.fut.as_mut().poll(cx) {
                Poll::Ready(res) => {
                    let res = res.unwrap_or_else(ResponseError::response_error);
                    return Poll::Ready(Ok(res));
                }
                // service call is pending. could be waiting for more read.
                Poll::Pending => {
                    if this.io.poll_read_decode_body(this.body_handle, cx)? {
                        continue;
                    } else {
                        return Poll::Pending;
                    }
                }
            }
        }
    }
}

pin_project! {
    struct ResponseHandler<'a, 'b, St, ResB> {
        #[pin]
        res_body: ResponseBody<ResB>,
        encoder: TransferEncoding,
        body_handle: Option<RequestBodyHandle>,
        io: &'a mut Io<'b, St>,
    }
}

impl<St, ResB, E> Future for ResponseHandler<'_, '_, St, ResB>
where
    ResB: Stream<Item = Result<Bytes, E>>,
    BodyError: From<E>,

    St: AsyncStream,
{
    type Output = Result<(), Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();

        let io = this.io;

        loop {
            match this.res_body.as_mut().poll_next(cx) {
                Poll::Ready(Some(bytes)) => {
                    let bytes = bytes?;
                    this.encoder.encode(bytes, &mut io.write_buf)?;
                }
                Poll::Ready(None) => {
                    this.encoder.encode_eof(&mut io.write_buf)?;
                    return Poll::Ready(Ok(()));
                }
                // payload sending is pending.
                // it could be waiting for more read from client.
                Poll::Pending => {
                    // write buffer to client so it can feed us new
                    // chunked requests if there is any.
                    if io.io.poll_write_ready(cx)?.is_ready() {
                        let _ = io.try_write()?;
                    }

                    if io.poll_read_decode_body(this.body_handle, cx)? {
                        continue;
                    } else {
                        return Poll::Pending;
                    }
                }
            }
        }
    }
}

type DecodedHead = (Request<RequestBody>, Option<RequestBodyHandle>);

struct RequestBodyHandle {
    decoder: RequestBodyDecoder,
    sender: RequestBodySender,
}

impl RequestBodyHandle {
    fn new_pair(decoder: RequestBodyDecoder) -> (Option<RequestBodyHandle>, RequestBody) {
        if decoder.is_eof() {
            let (_, body) = RequestBody::create(true);
            (None, body)
        } else {
            let (sender, body) = RequestBody::create(false);
            let body_handle = RequestBodyHandle { decoder, sender };
            (Some(body_handle), body)
        }
    }

    fn close(&mut self) {
        self.sender.feed_eof();
    }
}
