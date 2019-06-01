//! Internal state for ARQ connections

use std::cmp::min;
use std::collections::vec_deque::VecDeque;
use std::fmt;
use std::io;
use std::io::{Cursor, Read};
use std::pin::Pin;
use std::time::{Duration, Instant};

use bytes::{Buf, Bytes};

use num::Integer;

use futures::sink::Sink;
use futures::stream::Stream;
use futures::task::{Context, Poll};

use crate::connectioninfo::ConnectionInfo;
use crate::protocol::response::ConnectionStateChange;
use crate::tncdata::{DataIn, DataOut};
use crate::tncio::dataevent::DataEvent;

const INITIAL_NUM_BUF: usize = 16;
const SEND_HWM: u64 = 65535;

/// State and buffers for an ARQ connection
///
/// This object holds receiving buffers and tracks the
/// progress of transmitted bytes on to their final
/// destination—i.e., the remote peer. This object
/// performs async read and write operations, but it
/// does not hold the I/O resources or implement any
/// of the `Async...` traits.
pub struct ArqState {
    info: ConnectionInfo,
    rx_buffers: VecDeque<Cursor<Bytes>>,
    open_time: Instant,
    final_elapsed_time: Option<Duration>,
    closed_read: bool,
    closed_write: bool,
    bytecount_rx: u64,
    bytecount_tx: u64,
    bytecount_tx_staged: u64,
    bytecount_tx_unacknowledged: u64,
    last_reported_buffer: u64,
}

impl ArqState {
    /// New ARQ connection state
    ///
    /// # Parameters
    /// - `info`: Metadata about this connection, such as
    ///   the source and destination callsigns. The metadata
    ///   is immutable and constant for the duration of this
    ///   `ArqState`.
    pub fn new(info: ConnectionInfo) -> Self {
        ArqState {
            info,
            rx_buffers: VecDeque::with_capacity(INITIAL_NUM_BUF),
            open_time: Instant::now(),
            final_elapsed_time: None,
            closed_read: false,
            closed_write: false,
            bytecount_rx: 0,
            bytecount_tx: 0,
            bytecount_tx_staged: 0,
            bytecount_tx_unacknowledged: 0,
            last_reported_buffer: 0,
        }
    }

    /// True if the connection was open (at last check)
    ///
    /// This method returns `true` if the connection was
    /// believed to be open during the last I/O operation
    /// conducted to the ARDOP TNC.
    ///
    /// Even if this value returns `true`, the connection
    /// may be detected as dead during the next read or
    /// write.
    pub fn is_open(&self) -> bool {
        self.final_elapsed_time.is_none()
    }

    /// Return connection information
    ///
    /// Includes immutable details about the connection, such
    /// as the local and remote callsigns.
    pub fn info(&self) -> &ConnectionInfo {
        &self.info
    }

    /// Returns total number of bytes received
    ///
    /// Counts the total number of *payload* bytes which have
    /// been transmitted over the air *AND* acknowledged by
    /// the remote peer. This value is aggregated over the
    /// lifetime of the `ArqStream`.
    pub fn bytes_received(&self) -> u64 {
        self.bytecount_rx
    }

    /// Total number of bytes successfully transmitted
    ///
    /// Counts the total number of *payload* bytes which have
    /// been transmitted over the air *AND* acknowledged by
    /// the remote peer. This value is aggregated over the
    /// lifetime of the `ArqStream`.
    pub fn bytes_transmitted(&self) -> u64 {
        self.bytecount_tx
    }

    /// Total number of bytes pending peer acknowledgement
    ///
    /// Counts the total number of bytes that have been
    /// accepted by the local ARDOP TNC but have not yet
    /// been delivered to the peer.
    ///
    /// Bytes accepted by this object become *staged*. Once
    /// the TNC has accepted responsibility for the bytes,
    /// they become *unacknowledged*. Once the remote peer
    /// has acknowledged the data, the bytes become
    /// *transmitted*.
    pub fn bytes_unacknowledged(&self) -> u64 {
        self.bytecount_tx_unacknowledged
    }

    /// Bytes pending acceptance by the local TNC
    ///
    /// Counts the total number of bytes which have been
    /// accepted by this object internally but have not
    /// yet been delivered to the TNC for transmission.
    ///
    /// Bytes accepted by this object become *staged*. Once
    /// the TNC has accepted responsibility for the bytes,
    /// they become *unacknowledged*. Once the remote peer
    /// has acknowledged the data, the bytes become
    /// *transmitted*.
    pub fn bytes_staged(&self) -> u64 {
        self.bytecount_tx_staged
    }

    /// Returns total time elapsed while the connection is/was open
    ///
    /// Returns the total time, in a monotonic reference frame,
    /// elapsed between
    /// 1. the connection being opened; and
    /// 2. the connection being closed
    /// If the connection is still open, then (2) is assumed to be
    /// `now`.
    ///
    /// # Return
    /// Time elapsed since connection was open
    pub fn elapsed_time(&self) -> Duration {
        if self.is_open() {
            self.open_time.elapsed()
        } else {
            self.final_elapsed_time.unwrap() // checked
        }
    }

    /// Mark this connection as closed for reading and writing
    ///
    /// Indicate that the disconnect process has concluded.
    /// No more data will be accepted for writing, and only
    /// data that has already been retrieved from the ARDOP TNC
    /// will be presented for reading.
    ///
    /// This method does not and cannot start a disconnect.
    /// Higher-level logic is responsible for this behavior.
    pub fn shutdown_read(&mut self) {
        self.mark_closed();
    }

    /// Mark this connection as closed for writing
    ///
    /// Indicate that the *local* side has started the disconnect
    /// process. No more data will be accepted for writing, but
    /// there may still be unread (or even untransmitted) data
    /// remaining from the remote peer.
    ///
    /// This method does not and cannot start a disconnect.
    /// Higher-level logic is responsible for this behavior.
    pub fn shutdown_write(&mut self) {
        self.closed_write = true;
    }

    /// Attempts to read bytes into `buf`
    ///
    /// Reads bytes from the following sources:
    /// 1. The internal buffers in this object.
    /// 2. If insufficient, the unpinned stream `src` is
    ///    read
    ///
    /// # Parameters
    /// - `src`: Source of `DataEvent`
    /// - `cx`: Polling context
    /// - `buf`: Destination buffer for bytes
    ///
    /// # Return
    /// A count of bytes copied into `buf`, or an `io::Error`.
    /// At present, errors only occur if the connection to the
    /// local ARDOP TNC has been lost. Broken TNC connections
    /// will raise a `io::ErrorKind:L:ConnectionReset` error.
    ///
    /// This method is designed for compatibility with the
    /// `AsyncRead` trait, but it does not implement it.
    pub fn poll_read<S>(
        &mut self,
        src: &mut S,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>>
    where
        S: Stream<Item = DataEvent> + Unpin,
    {
        let mut total_read = 0usize;
        loop {
            // read from the internal buffers, first
            total_read += read_from_buffers(&mut self.rx_buffers, &mut buf[total_read..]);
            if total_read >= buf.len() || self.closed_read {
                // request satisfied using just the buffer,
                // or there is no more to read
                break;
            }

            match self.poll_next_dataevent(src, cx, true) {
                // no more yet
                Poll::Pending => break,
                // lost connection to ARDOP
                Poll::Ready(Err(x)) => return Poll::Ready(Err(x)),
                // more
                Poll::Ready(Ok(())) => continue,
            }
        }

        if total_read > 0 {
            self.bytecount_rx += total_read as u64;
            Poll::Ready(Ok(total_read))
        } else if self.closed_read {
            Poll::Ready(Ok(0usize))
        } else {
            Poll::Pending
        }
    }

    /// Attempt to write bytes from `buf`
    ///
    /// Attempts to transmit the bytes in `buf` to the remote
    /// peer. This method will reject send requests with
    /// `Poll::Pending` if the number of buffered bytes
    /// exceeds the sending *high-water mark*. Errors will be
    /// raised if the connection is closed.
    ///
    /// Writes are always atomic. Either all of `buf` will be
    /// sent to the TNC for transmission, or none of `buf` will
    /// be sent to the TNC for transmission.
    ///
    /// # Parameters
    /// - `dst`: Sink for outgoing data
    /// - `src`: Source of incoming events and data. Needed to
    ///   update the outgoing `BUFFER` size
    /// - `cx`: Async Context
    /// - `buf`: Payload data to send
    ///
    /// # Returns
    /// Writes to a closed ARQ connection will raise a `BrokenPipe`
    /// IO error. Writes to a broken local TNC connection will raise
    /// a `ConnectionReset` error.
    ///
    /// If this method returns `Poll::Pending`, the TNC's outgoing
    /// buffer is full, and the send cannot proceed. If this method
    /// returns `Poll::Ready`, then the entirety of `buf` has been
    /// accepted for transmission.
    ///
    /// Note that this method does not guarantee that the bytes have
    /// been, or ever will be, delivered to the remote peer.
    pub fn poll_write<K, S>(
        &mut self,
        dst: &mut K,
        src: &mut S,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>>
    where
        K: Sink<DataOut> + Unpin,
        S: Stream<Item = DataEvent> + Unpin,
    {
        if self.closed_write {
            return Poll::Ready(Err(broken_pipe_err()));
        }

        if self.bytecount_tx_unacknowledged + self.bytecount_tx_staged > SEND_HWM {
            // Too much data queued.
            debug!(target:"ARQ", "Inhibiting write while buffer longer than SEND_HWM");

            // Try to flush. If we haven't flushed, then
            // apply backpressure and don't accept any more
            // bytes.
            ready!(self.poll_flush(dst, src, cx))?;
        }

        // check if the outgoing framer is ready for more data
        // returns Poll::Pending if not
        match ready!(Pin::new(&mut *dst).poll_ready(cx)) {
            Ok(_ok) => (),
            Err(_err) => return Poll::Ready(Err(connection_reset_err())),
        }

        // enqueue the bytes for sending
        let bytes_out = Bytes::from(buf);
        let bytes_len = bytes_out.len();
        match Pin::new(&mut *dst).start_send(bytes_out) {
            Ok(_ok) => (),
            Err(_err) => return Poll::Ready(Err(connection_reset_err())),
        }
        self.bytecount_tx_staged += bytes_len as u64;

        // try to flush the bytes out of the TCP connection
        match Pin::new(&mut *dst).poll_flush(cx) {
            Poll::Pending => Poll::Ready(Ok(bytes_len as usize)),
            Poll::Ready(Ok(())) => Poll::Ready(Ok(bytes_len as usize)),
            Poll::Ready(Err(_e)) => Poll::Ready(Err(connection_reset_err())),
        }
    }

    /// Poll for data transmission to the peer to complete
    ///
    /// This method will return `Poll::Pending` until all buffered
    /// data has been transmitted to the remote peer *or* the
    /// connection has failed or dropped.
    ///
    /// # Parameters
    /// - `dst`: Sink for outgoing data
    /// - `src`: Source of incoming events and data. Needed to
    ///   update the outgoing `BUFFER` size
    /// - `cx`: Async Context
    ///
    /// # Returns
    /// Writes to a closed ARQ connection will raise a `BrokenPipe`
    /// IO error. Writes to a broken local TNC connection will raise
    /// a `ConnectionReset` error.
    ///
    /// If this method returns `Poll::Pending`, the TNC's outgoing
    /// buffer is full, and the send cannot proceed. If this method
    /// returns `Poll::Ready`, then the entirety of `buf` has been
    /// accepted for transmission.
    pub fn poll_flush<K, S>(
        &mut self,
        dst: &mut K,
        src: &mut S,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<()>>
    where
        K: Sink<DataOut> + Unpin,
        S: Stream<Item = DataEvent> + Unpin,
    {
        loop {
            if self.closed_read {
                return Poll::Ready(Err(broken_pipe_err()));
            }
            if self.bytecount_tx_unacknowledged + self.bytecount_tx_staged == 0 {
                // we are all flushed
                return Poll::Ready(Ok(()));
            }

            // Try to flush some data out of TCP connection
            if self.bytecount_tx_staged > 0 {
                match ready!(Pin::new(&mut *dst).poll_flush(cx)) {
                    Ok(_ok) => (),
                    Err(_err) => return Poll::Ready(Err(connection_reset_err())),
                }
            }

            // Try to update the tx byte counts
            ready!(self.poll_next_dataevent(src, cx, false))?;

            if self.bytecount_tx_unacknowledged + self.bytecount_tx_staged <= 0 {
                debug!(target:"ARQ", "All buffered data flushed to peer.");
                return Poll::Ready(Ok(()));
            }
        }
    }

    // Attempt to read the next DataEvent from the given Stream.
    //
    // Reads data into the rxbuffers and processes events.
    // Returns Poll::Ready if new data is available, or
    // Poll::Pending if no data is available.
    fn poll_next_dataevent<S>(
        &mut self,
        src: &mut S,
        cx: &mut Context<'_>,
        data_only: bool,
    ) -> Poll<io::Result<()>>
    where
        S: Stream<Item = DataEvent> + Unpin,
    {
        // read more from the connection
        let next_item = ready!(Pin::new(src).poll_next(cx));
        match next_item {
            None => {
                error!(target: "ARQ", "Lost connection to local ARDOP TNC");
                self.mark_closed();
                Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::ConnectionReset,
                    "Lost connection to ARDOP TNC",
                )))
            }
            Some(DataEvent::Event(evt)) => {
                self.handle_event(evt);
                if data_only {
                    Poll::Pending
                } else {
                    Poll::Ready(Ok(()))
                }
            }
            Some(DataEvent::Data(DataIn::FEC(_data))) => {
                /* drop FEC data on the floor */
                Poll::Pending
            }
            Some(DataEvent::Data(DataIn::ARQ(data))) => {
                // append fresh data to buffer set
                self.rx_buffers.push_back(Cursor::new(data));
                Poll::Ready(Ok(()))
            }
        }
    }

    // processes a connection-relevant event
    fn handle_event(&mut self, event: ConnectionStateChange) {
        match event {
            ConnectionStateChange::Closed => {
                // This connection has gone away.
                // It has ceased to be.
                // This is an EX CONNECTION.
                self.mark_closed();
            }
            ConnectionStateChange::SendBuffer(newbuf) => {
                if newbuf < self.last_reported_buffer {
                    // bytes_ack bytes have been ACK'd by the peer
                    let bytes_ack = self.last_reported_buffer - newbuf;

                    // decrease the outstanding byte count by bytes_ack,
                    // and increase the success byte count by bytes_ack,
                    // ensuring that we do not overflow
                    let bytes_done = min(bytes_ack, self.bytecount_tx_unacknowledged);
                    self.bytecount_tx_unacknowledged -= bytes_done;
                    self.bytecount_tx += bytes_done;
                    debug!(target: "ARQ", "Peer ACK'd {} bytes", bytes_done);
                } else {
                    let bytes_accpt =
                        min(self.bytecount_tx_staged, newbuf - self.last_reported_buffer);
                    self.bytecount_tx_unacknowledged += bytes_accpt;
                    self.bytecount_tx_staged -= bytes_accpt;
                    debug!(target: "ARQ", "TNC accepted {} bytes", bytes_accpt);
                }
                self.last_reported_buffer = newbuf;
            }
            _ => { /* no-op */ }
        }
    }

    // mark this connection as closed
    fn mark_closed(&mut self) {
        info!(target: "ARQ", "Connection to {} CLOSED", self.info.peer_call());
        info!(target: "ARQSTATUS", "{}", &self);
        self.closed_read = true;
        self.closed_write = true;
        self.final_elapsed_time = Some(self.open_time.elapsed());
    }
}

impl fmt::Display for ArqState {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        const OPEN_CLOSED_MARK: &'static [&'static str] = &["=", "+"];

        let tx_kib = self.bytecount_tx as f32 / 1024.0f32;
        let rx_kib = self.bytecount_rx as f32 / 1024.0f32;
        let open_mark = &OPEN_CLOSED_MARK[self.is_open() as usize];
        let elapsed_secs = self.elapsed_time().as_secs();
        let (minutes, seconds) = elapsed_secs.div_rem(&60);

        write!(
            f,
            "{} [{}{:04}m{:02}s]: Rx:{} KiB, Tx:{} KiB",
            self.info, open_mark, minutes, seconds, rx_kib, tx_kib
        )
    }
}

impl Unpin for ArqState {}

fn connection_reset_err() -> io::Error {
    io::Error::new(
        io::ErrorKind::ConnectionReset,
        "Lost connection to ARDOP TNC",
    )
}
fn broken_pipe_err() -> io::Error {
    io::Error::new(
        io::ErrorKind::BrokenPipe,
        "Broken pipe: cannot write to a closed connection",
    )
}

// try to fill dst from buffers
fn read_from_buffers(buffers: &mut VecDeque<Cursor<Bytes>>, dst: &mut [u8]) -> usize {
    let mut total_read = 0usize;
    while !buffers.is_empty() && total_read < dst.len() {
        if !buffers.front().unwrap().has_remaining() {
            // this buffer is empty. next.
            let _ = buffers.pop_front();
            continue;
        }

        // unwrap safe (checked)
        let head = buffers.front_mut().unwrap();

        // unwrap safe (I/O on cursor always succeeds)
        let num_out = head.read(&mut dst[total_read..]).unwrap();
        total_read += num_out;
    }

    total_read
}

#[cfg(test)]
mod test {
    use super::*;

    use futures::channel::mpsc;
    use futures::sink;
    use futures::stream;
    use futures::task;

    use crate::connectioninfo::Direction;

    #[test]
    fn test_read_from_buffers() {
        let b1 = Bytes::from(vec![0u8, 1u8, 2u8]);
        let b2 = Bytes::from(vec![]);
        let b3 = Bytes::from(vec![3u8, 4u8, 5u8, 6u8]);

        let mut bufs: VecDeque<Cursor<Bytes>> = VecDeque::with_capacity(3);
        bufs.push_back(Cursor::new(b1));
        bufs.push_back(Cursor::new(b2));
        bufs.push_back(Cursor::new(b3));

        let mut out1 = [0u8; 2usize];
        assert_eq!(2, read_from_buffers(&mut bufs, &mut out1));
        assert_eq!(out1, [0u8, 1u8]);

        let mut out2 = [0u8; 4usize];
        assert_eq!(4, read_from_buffers(&mut bufs, &mut out2));
        assert_eq!(out2, [2u8, 3u8, 4u8, 5u8]);

        assert_eq!(1, read_from_buffers(&mut bufs, &mut out2));
        assert_eq!(out2, [6u8, 3u8, 4u8, 5u8]);

        assert_eq!(0, read_from_buffers(&mut bufs, &mut out2));
        assert_eq!(out2, [6u8, 3u8, 4u8, 5u8]);
    }

    #[test]
    fn test_poll_receive() {
        let nfo = ConnectionInfo::new(
            "W1AW",
            Some("EM00".to_owned()),
            500,
            Direction::Outgoing("W9ABC".to_owned()),
        );
        let mut arq = ArqState::new(nfo);

        // stream
        let de = vec![
            DataEvent::Data(DataIn::ARQ(Bytes::from_static(b"HELLO "))),
            DataEvent::Data(DataIn::ARQ(Bytes::from_static(b"WORLD!"))),
            DataEvent::Event(ConnectionStateChange::Closed),
        ];
        let mut instream = stream::iter(de);

        let mut out = [0u8; 8];
        let mut waker = Context::from_waker(task::noop_waker_ref());

        // read first fragment
        match arq.poll_read(&mut instream, &mut waker, &mut out) {
            Poll::Ready(Ok(8)) => assert!(true),
            _ => assert!(false),
        }
        assert_eq!(*b"HELLO WO", out);
        assert!(arq.is_open());
        assert_eq!(8, arq.bytes_received());

        // read second fragment
        match arq.poll_read(&mut instream, &mut waker, &mut out) {
            Poll::Ready(Ok(4)) => assert!(true),
            _ => assert!(false),
        }
        assert_eq!(*b"RLD!", &out[0..4]);
        assert_eq!(false, arq.is_open());
        assert_eq!(12, arq.bytes_received());

        // additional reads are EOF
        match arq.poll_read(&mut instream, &mut waker, &mut out) {
            Poll::Ready(Ok(0)) => assert!(true),
            _ => assert!(false),
        }
    }

    #[test]
    fn test_poll_write() {
        let nfo = ConnectionInfo::new(
            "W1AW",
            Some("EM00".to_owned()),
            500,
            Direction::Outgoing("W9ABC".to_owned()),
        );
        let mut arq = ArqState::new(nfo);
        let mut waker = Context::from_waker(task::noop_waker_ref());

        let (evt_wr, mut evt_rd) = mpsc::unbounded();
        let mut data_sink = sink::drain(); // om nom nom

        let res = arq.poll_write(&mut data_sink, &mut evt_rd, &mut waker, b"HELLO");

        // our mock TNC has consumed the bytes... but not transmitted them yet
        match res {
            Poll::Ready(Ok(5)) => assert!(true),
            _ => assert!(false),
        }
        assert_eq!(5, arq.bytes_staged());
        assert_eq!(0, arq.bytes_unacknowledged());
        assert_eq!(0, arq.bytes_transmitted());

        // flushes fail to make progress
        match arq.poll_flush(&mut data_sink, &mut evt_rd, &mut waker) {
            Poll::Pending => assert!(true),
            _ => assert!(false),
        }
        assert_eq!(5, arq.bytes_staged());
        assert_eq!(0, arq.bytes_unacknowledged());
        assert_eq!(0, arq.bytes_transmitted());

        // the tnc accepts the bytes, but hasn't sent any
        evt_wr
            .unbounded_send(DataEvent::Event(ConnectionStateChange::SendBuffer(5)))
            .unwrap();
        match arq.poll_flush(&mut data_sink, &mut evt_rd, &mut waker) {
            Poll::Pending => assert!(true),
            _ => assert!(false),
        }
        assert_eq!(0, arq.bytes_staged());
        assert_eq!(5, arq.bytes_unacknowledged());
        assert_eq!(0, arq.bytes_transmitted());

        // the tnc sends some bytes
        evt_wr
            .unbounded_send(DataEvent::Event(ConnectionStateChange::SendBuffer(3)))
            .unwrap();
        match arq.poll_flush(&mut data_sink, &mut evt_rd, &mut waker) {
            Poll::Pending => assert!(true),
            _ => assert!(false),
        }
        assert_eq!(0, arq.bytes_staged());
        assert_eq!(3, arq.bytes_unacknowledged());
        assert_eq!(2, arq.bytes_transmitted());

        // report all flushed
        evt_wr
            .unbounded_send(DataEvent::Event(ConnectionStateChange::SendBuffer(0)))
            .unwrap();
        match arq.poll_flush(&mut data_sink, &mut evt_rd, &mut waker) {
            Poll::Ready(Ok(())) => assert!(true),
            _ => assert!(false),
        }
        assert_eq!(0, arq.bytes_staged());
        assert_eq!(0, arq.bytes_unacknowledged());
        assert_eq!(5, arq.bytes_transmitted());

        // mark write shutdown... but we haven't received a
        // disconnect confirmation yet, so we're still open
        arq.shutdown_write();
        match arq.poll_flush(&mut data_sink, &mut evt_rd, &mut waker) {
            Poll::Ready(Ok(_o)) => assert!(true),
            _ => assert!(false),
        }
        assert!(arq.is_open());

        // Send confirmation of disconnect. Now we are closed.
        evt_wr
            .unbounded_send(DataEvent::Event(ConnectionStateChange::Closed))
            .unwrap();

        // We're still flushed
        match arq.poll_flush(&mut data_sink, &mut evt_rd, &mut waker) {
            Poll::Ready(Ok(_o)) => assert!(true),
            _ => assert!(false),
        }

        // No more bytes are accepted
        let res = arq.poll_write(&mut data_sink, &mut evt_rd, &mut waker, b"HELLO");
        match res {
            Poll::Ready(Err(_e)) => assert!(true),
            _ => assert!(false),
        }
    }
}
