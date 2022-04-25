use crate::frame::{self, Frame};

use bytes::{Buf, BytesMut};
use std::{
    future::Future,
    io::{self, Cursor},
    pin::Pin,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufWriter};
use tokio_uring::{
    buf::{IoBuf, IoBufMut},
    net::TcpStream,
};

/// Send and receive `Frame` values from a remote peer.
///
/// When implementing networking protocols, a message on that protocol is
/// often composed of several smaller messages known as frames. The purpose of
/// `Connection` is to read and write frames on the underlying `TcpStream`.
///
/// To read frames, the `Connection` uses an internal buffer, which is filled
/// up until there are enough bytes to create a full frame. Once this happens,
/// the `Connection` creates the frame and returns it to the caller.
///
/// When sending frames, the frame is first encoded into the write buffer.
/// The contents of the write buffer are then written to the socket.
// #[derive(Debug)]
pub struct Connection {
    // The `TcpStream`. It is decorated with a `BufWriter`, which provides write
    // level buffering. The `BufWriter` implementation provided by Tokio is
    // sufficient for our needs.
    stream: BufWriter<TcpStreamAdpator>,

    // The buffer for reading frames.
    buffer: BytesMut,
}

impl Connection {
    /// Create a new `Connection`, backed by `socket`. Read and write buffers
    /// are initialized.
    pub fn new(socket: TcpStream) -> Connection {
        let stream = TcpStreamAdpator {
            socket,
            read_fut: None,
            read_owned_buf: Some(Vec::with_capacity(1024)),
            write_fut: None,
            write_owned_buf: Some(Vec::with_capacity(1024)),
            write_len: 0,
        };
        Connection {
            stream: BufWriter::new(stream),
            // Default to a 4KB read buffer. For the use case of mini redis,
            // this is fine. However, real applications will want to tune this
            // value to their specific use case. There is a high likelihood that
            // a larger read buffer will work better.
            buffer: BytesMut::with_capacity(4 * 1024),
        }
    }

    /// Read a single `Frame` value from the underlying stream.
    ///
    /// The function waits until it has retrieved enough data to parse a frame.
    /// Any data remaining in the read buffer after the frame has been parsed is
    /// kept there for the next call to `read_frame`.
    ///
    /// # Returns
    ///
    /// On success, the received frame is returned. If the `TcpStream`
    /// is closed in a way that doesn't break a frame in half, it returns
    /// `None`. Otherwise, an error is returned.
    pub async fn read_frame(&mut self) -> crate::Result<Option<Frame>> {
        loop {
            // Attempt to parse a frame from the buffered data. If enough data
            // has been buffered, the frame is returned.
            if let Some(frame) = self.parse_frame()? {
                return Ok(Some(frame));
            }

            // There is not enough buffered data to read a frame. Attempt to
            // read more data from the socket.
            //
            // On success, the number of bytes is returned. `0` indicates "end
            // of stream".
            if 0 == self.stream.read_buf(&mut self.buffer).await? {
                // The remote closed the connection. For this to be a clean
                // shutdown, there should be no data in the read buffer. If
                // there is, this means that the peer closed the socket while
                // sending a frame.
                if self.buffer.is_empty() {
                    return Ok(None);
                } else {
                    return Err("connection reset by peer".into());
                }
            }
        }
    }

    /// Tries to parse a frame from the buffer. If the buffer contains enough
    /// data, the frame is returned and the data removed from the buffer. If not
    /// enough data has been buffered yet, `Ok(None)` is returned. If the
    /// buffered data does not represent a valid frame, `Err` is returned.
    fn parse_frame(&mut self) -> crate::Result<Option<Frame>> {
        use frame::Error::Incomplete;

        // Cursor is used to track the "current" location in the
        // buffer. Cursor also implements `Buf` from the `bytes` crate
        // which provides a number of helpful utilities for working
        // with bytes.
        let mut buf = Cursor::new(&self.buffer[..]);

        // The first step is to check if enough data has been buffered to parse
        // a single frame. This step is usually much faster than doing a full
        // parse of the frame, and allows us to skip allocating data structures
        // to hold the frame data unless we know the full frame has been
        // received.
        match Frame::check(&mut buf) {
            Ok(_) => {
                // The `check` function will have advanced the cursor until the
                // end of the frame. Since the cursor had position set to zero
                // before `Frame::check` was called, we obtain the length of the
                // frame by checking the cursor position.
                let len = buf.position() as usize;

                // Reset the position to zero before passing the cursor to
                // `Frame::parse`.
                buf.set_position(0);

                // Parse the frame from the buffer. This allocates the necessary
                // structures to represent the frame and returns the frame
                // value.
                //
                // If the encoded frame representation is invalid, an error is
                // returned. This should terminate the **current** connection
                // but should not impact any other connected client.
                let frame = Frame::parse(&mut buf)?;

                // Discard the parsed data from the read buffer.
                //
                // When `advance` is called on the read buffer, all of the data
                // up to `len` is discarded. The details of how this works is
                // left to `BytesMut`. This is often done by moving an internal
                // cursor, but it may be done by reallocating and copying data.
                self.buffer.advance(len);

                // Return the parsed frame to the caller.
                Ok(Some(frame))
            }
            // There is not enough data present in the read buffer to parse a
            // single frame. We must wait for more data to be received from the
            // socket. Reading from the socket will be done in the statement
            // after this `match`.
            //
            // We do not want to return `Err` from here as this "error" is an
            // expected runtime condition.
            Err(Incomplete) => Ok(None),
            // An error was encountered while parsing the frame. The connection
            // is now in an invalid state. Returning `Err` from here will result
            // in the connection being closed.
            Err(e) => Err(e.into()),
        }
    }

    /// Write a single `Frame` value to the underlying stream.
    ///
    /// The `Frame` value is written to the socket using the various `write_*`
    /// functions provided by `AsyncWrite`. Calling these functions directly on
    /// a `TcpStream` is **not** advised, as this will result in a large number of
    /// syscalls. However, it is fine to call these functions on a *buffered*
    /// write stream. The data will be written to the buffer. Once the buffer is
    /// full, it is flushed to the underlying socket.
    pub async fn write_frame(&mut self, frame: &Frame) -> io::Result<()> {
        // Arrays are encoded by encoding each entry. All other frame types are
        // considered literals. For now, mini-redis is not able to encode
        // recursive frame structures. See below for more details.
        match frame {
            Frame::Array(val) => {
                // Encode the frame type prefix. For an array, it is `*`.
                self.stream.write_u8(b'*').await?;

                // Encode the length of the array.
                self.write_decimal(val.len() as u64).await?;

                // Iterate and encode each entry in the array.
                for entry in &**val {
                    self.write_value(entry).await?;
                }
            }
            // The frame type is a literal. Encode the value directly.
            _ => self.write_value(frame).await?,
        }

        // Ensure the encoded frame is written to the socket. The calls above
        // are to the buffered stream and writes. Calling `flush` writes the
        // remaining contents of the buffer to the socket.
        self.stream.flush().await
    }

    /// Write a frame literal to the stream
    async fn write_value(&mut self, frame: &Frame) -> io::Result<()> {
        match frame {
            Frame::Simple(val) => {
                self.stream.write_u8(b'+').await?;
                self.stream.write_all(val.as_bytes()).await?;
                self.stream.write_all(b"\r\n").await?;
            }
            Frame::Error(val) => {
                self.stream.write_u8(b'-').await?;
                self.stream.write_all(val.as_bytes()).await?;
                self.stream.write_all(b"\r\n").await?;
            }
            Frame::Integer(val) => {
                self.stream.write_u8(b':').await?;
                self.write_decimal(*val).await?;
            }
            Frame::Null => {
                self.stream.write_all(b"$-1\r\n").await?;
            }
            Frame::Bulk(val) => {
                let len = val.len();

                self.stream.write_u8(b'$').await?;
                self.write_decimal(len as u64).await?;
                self.stream.write_all(val).await?;
                self.stream.write_all(b"\r\n").await?;
            }
            // Encoding an `Array` from within a value cannot be done using a
            // recursive strategy. In general, async fns do not support
            // recursion. Mini-redis has not needed to encode nested arrays yet,
            // so for now it is skipped.
            Frame::Array(_val) => unreachable!(),
        }

        Ok(())
    }

    /// Write a decimal frame to the stream
    async fn write_decimal(&mut self, val: u64) -> io::Result<()> {
        use std::io::Write;

        // Convert the value to a string
        let mut buf = [0u8; 20];
        let mut buf = Cursor::new(&mut buf[..]);
        write!(&mut buf, "{}", val)?;

        let pos = buf.position() as usize;
        self.stream.write_all(&buf.get_ref()[..pos]).await?;
        self.stream.write_all(b"\r\n").await?;

        Ok(())
    }
}

type PinnedFuture<B> = Pin<Box<dyn Future<Output = tokio_uring::BufResult<usize, B>> + 'static>>;

struct LimitedBuffer {
    buf: Vec<u8>,
    limit: usize,
}

unsafe impl IoBuf for LimitedBuffer {
    fn stable_ptr(&self) -> *const u8 {
        self.buf.as_ptr()
    }

    fn bytes_init(&self) -> usize {
        self.buf.len().min(self.limit)
    }

    fn bytes_total(&self) -> usize {
        self.buf.capacity().min(self.limit)
    }
}

unsafe impl IoBufMut for LimitedBuffer {
    fn stable_mut_ptr(&mut self) -> *mut u8 {
        self.buf.as_mut_ptr()
    }

    unsafe fn set_init(&mut self, pos: usize) {
        self.buf.set_init(pos)
    }
}

pub struct TcpStreamAdpator {
    socket: TcpStream,

    read_fut: Option<PinnedFuture<LimitedBuffer>>,
    read_owned_buf: Option<Vec<u8>>,

    write_fut: Option<PinnedFuture<Vec<u8>>>,
    write_owned_buf: Option<Vec<u8>>,
    write_len: usize,
}

impl AsyncRead for TcpStreamAdpator {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        let this = self.get_mut();
        let mut fut = this.read_fut.take().unwrap_or_else(|| {
            // we can read at most that length
            let mut owned_buf = this.read_owned_buf.take().unwrap();
            let cap = owned_buf.capacity();
            let remaining = buf.remaining();
            if remaining > cap {
                owned_buf.reserve(remaining - cap); //  remain - cap
            }
            let limited_buffer = LimitedBuffer {
                buf: owned_buf,
                limit: remaining,
            };
            let socket = unsafe { &*(&this.socket as *const TcpStream) };
            let f = TcpStream::read(socket, limited_buffer);
            Box::pin(f)
        });
        match fut.as_mut().poll(cx) {
            std::task::Poll::Ready((r, limited_buffer)) => {
                let inner = limited_buffer.buf;
                let ret = if let Err(e) = r {
                    std::task::Poll::Ready(Err(e))
                } else {
                    buf.put_slice(&inner[..r.unwrap()]);
                    std::task::Poll::Ready(Ok(()))
                };
                this.read_owned_buf = Some(inner);
                ret
            }
            std::task::Poll::Pending => {
                this.read_fut = Some(fut);
                std::task::Poll::Pending
            }
        }
    }
}

impl AsyncWrite for TcpStreamAdpator {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize, io::Error>> {
        let this = self.get_mut();
        let mut fut = this.write_fut.take().unwrap_or_else(|| {
            let mut owned_buf = this.write_owned_buf.take().unwrap();
            let stream = unsafe { &*(&this.socket as *const TcpStream) };

            owned_buf.clear();
            owned_buf.extend_from_slice(buf);

            this.write_len = owned_buf.len();
            let f = TcpStream::write(stream, owned_buf);
            Box::pin(f)
        });
        // Check if the slice between different poll_write calls is the same
        if buf.len() != this.write_len {
            panic!("write slice length mismatch between poll_write");
        }
        match fut.as_mut().poll(cx) {
            std::task::Poll::Ready((r, owned_buf)) => {
                this.write_owned_buf = Some(owned_buf);
                match r {
                    Ok(n) => std::task::Poll::Ready(Ok(n)),
                    Err(e) => std::task::Poll::Ready(Err(e)),
                }
            }
            std::task::Poll::Pending => {
                this.write_fut = Some(fut);
                std::task::Poll::Pending
            }
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
        std::task::Poll::Ready(Ok(()))
    }
}
