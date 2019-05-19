//! Asynchronous ARDOP TNC backend
//!
//! This module contains the "meat" of the ARDOP TNC
//! interface. This object is asynchronous and returns
//! futures for all blocking operations. It also includes
//! management of ARQ connections.
//!
//! Higher-level objects will separate control functions
//! and ARQ connections into separate objects.
//!
use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::string::String;
use std::sync::Arc;
use std::time::Duration;

use futures::channel::mpsc;
use futures::io::{AsyncRead, AsyncWrite};
use futures::lock::Mutex;
use futures::sink::{Sink, SinkExt};
use futures::stream::{Stream, StreamExt};
use futures::task::SpawnExt;

use async_timer::Timed;

use romio::TcpStream;

use super::busylock;
use super::controlstream;
use super::controlstream::{ControlSink, ControlStreamResults};
use super::dataevent::{DataEvent, DataEventStream};

use crate::connectioninfo::ConnectionInfo;
use crate::framing::data::TncDataFraming;
use crate::framing::framer::Framed;
use crate::protocol::command;
use crate::protocol::command::Command;
use crate::protocol::constants::ProtocolMode;
use crate::protocol::response::{
    CommandOk, CommandResult, ConnectionFailedReason, ConnectionStateChange, Event,
};
use crate::tncdata::DataOut;
use crate::tncerror::{TncError, TncResult};

// Offset between control port and data port
const DATA_PORT_OFFSET: u16 = 1;

// Set TCP keepalive pings
const TCP_KEEPALIVE: Duration = Duration::from_secs(15);

// Default timeout for local TNC commands
const DEFAULT_TIMEOUT_COMMAND: Duration = Duration::from_millis(2000);

// Default timeout for TNC event resolution, such as connect or disconnect
const DEFAULT_TIMEOUT_EVENT: Duration = Duration::from_secs(90);

/// Asynchronous ARDOP TNC
///
/// This object communicates with the ARDOP program
/// via TCP, and it holds all I/O resources for this
/// task.
pub struct AsyncTnc<I, P>
where
    I: AsyncRead + AsyncWrite + Unpin + Send + Sync,
    P: SpawnExt,
{
    spawner: P,
    data_stream: DataEventStream<Framed<I, TncDataFraming>, mpsc::UnboundedReceiver<Event>>,
    control_in_res: ControlStreamResults<I>,
    control_out: ControlSink<I>,
    busy_mutex: Arc<Mutex<()>>,
    control_timeout: Duration,
    event_timeout: Duration,
}

impl<I: 'static, P> AsyncTnc<I, P>
where
    I: AsyncRead + AsyncWrite + Unpin + Send + Sync,
    P: SpawnExt,
{
    /// Connect to an ARDOP TNC
    ///
    /// Returns a future which will connect to an ARDOP TNC
    /// and initialize it.
    ///
    /// # Parameters
    /// - `spawner`: A `LocalPool` or a `ThreadPool` which will be
    ///   used to spawn TNC-related tasks.
    /// - `control_addr`: Network address of the ARDOP TNC's
    ///   control port.
    /// - `mycall`: The formally-assigned callsign for your station.
    ///   Legitimate call signs include from 3 to 7 ASCII characters
    ///   (A-Z, 0-9) followed by an optional "`-`" and an SSID of
    ///   `-0` to `-15` or `-A` to `-Z`. An SSID of `-0` is treated
    ///   as no SSID.
    /// - `min_clear_time`: Minimum duration for channel to be clear
    ///   in order for outgoing connection requests to be made.
    ///
    /// # Returns
    /// A new `AsyncTnc`, or an error if the connection or
    /// initialization step fails.
    pub async fn new<S>(
        mut spawner: P,
        control_addr: &SocketAddr,
        mycall: S,
        min_clear_time: Duration,
    ) -> TncResult<AsyncTnc<TcpStream, P>>
    where
        S: Into<String>,
        P: SpawnExt,
    {
        let data_addr = SocketAddr::new(control_addr.ip(), control_addr.port() + DATA_PORT_OFFSET);

        // connect
        let stream_control: TcpStream = TcpStream::connect(control_addr).await?;
        let stream_data: TcpStream = TcpStream::connect(&data_addr).await?;

        // set keepalive
        stream_control
            .set_keepalive(Some(TCP_KEEPALIVE.clone()))
            .expect("Can't set keepalive on control socket");
        stream_data
            .set_keepalive(Some(TCP_KEEPALIVE.clone()))
            .expect("Can't set keepalive on data socket");

        let mut out = AsyncTnc::new_from_streams(
            spawner,
            stream_control,
            stream_data,
            mycall,
            min_clear_time,
        );

        // Try to initialize the TNC. If we fail here, we will bail
        // with an error instead.
        match out.initialize().await {
            Ok(()) => {
                info!(target:"tnc", "Initialized ARDOP TNC at {}", &control_addr);
                Ok(out)
            }
            Err(e) => {
                error!(
                    target: "tnc",
                    "Unable to initialize ARDOP TNC at {}: {}",
                    &control_addr, &e
                );
                Err(e)
            }
        }
    }

    /// New from raw I/O types
    ///
    /// # Parameters
    /// - `spawner`: A `LocalPool` or a `ThreadPool` which will be
    ///   used to spawn TNC-related tasks.
    /// - `stream_control`: I/O stream to the TNC's control port
    /// - `stream_data`: I/O stream to the TNC's data port
    /// - `mycall`: The formally-assigned callsign for your station.
    ///   Legitimate call signs include from 3 to 7 ASCII characters
    ///   (A-Z, 0-9) followed by an optional "`-`" and an SSID of
    ///   `-0` to `-15` or `-A` to `-Z`. An SSID of `-0` is treated
    ///   as no SSID.
    /// - `min_clear_time`: Minimum duration for channel to be clear
    ///   in order for outgoing connection requests to be made.
    ///
    /// # Returns
    /// A new `AsyncTnc`. This method may panic if the `spawner`
    /// cannot spawn new tasks.
    pub(crate) fn new_from_streams<S>(
        mut spawner: P,
        stream_control: I,
        stream_data: I,
        mycall: S,
        min_clear_time: Duration,
    ) -> AsyncTnc<I, P>
    where
        S: Into<String>,
    {
        // create receiver streams for the control port
        let (control_in_evt, control_in_res, control_out) =
            controlstream::controlstream(stream_control);

        // spawn busy-detector task
        let busy_mutex = Arc::new(Mutex::new(()));
        let (busydet_tx, busydet_rx) = mpsc::unbounded();

        spawner
            .spawn(busylock::busy_lock_task(
                control_in_evt,
                busydet_tx,
                busy_mutex.clone(),
                min_clear_time,
                None,
            ))
            .expect("Unable to spawn busy-channel detection task");

        // create data port input/output framer
        let data_inout = Framed::new(stream_data, TncDataFraming::new());

        AsyncTnc {
            spawner,
            data_stream: DataEventStream::new(mycall, data_inout, busydet_rx),
            control_in_res,
            control_out,
            busy_mutex,
            control_timeout: DEFAULT_TIMEOUT_COMMAND,
            event_timeout: DEFAULT_TIMEOUT_EVENT,
        }
    }

    /// Get this station's callsign
    ///
    /// # Returns
    /// The formally assigned callsign for this station.
    pub fn mycall(&self) -> &String {
        self.data_stream.state().mycall()
    }

    /// Gets the control connection timeout value
    ///
    /// Commands sent via the `command()` method will
    /// timeout if either the send or receive takes
    /// longer than `timeout`.
    ///
    /// Timeouts cause `TncError::CommandTimeout` errors.
    /// If a command times out, there is likely a serious
    /// problem with the ARDOP TNC or its connection.
    ///
    /// # Returns
    /// Current timeout value
    pub fn control_timeout(&self) -> &Duration {
        &self.control_timeout
    }

    /// Sets timeout for the control connection
    ///
    /// Commands sent via the `command()` method will
    /// timeout if either the send or receive takes
    /// longer than `timeout`.
    ///
    /// # Parameters
    /// - `timeout`: New command timeout value
    pub fn set_control_timeout(&mut self, timeout: Duration) {
        self.control_timeout = timeout;
    }

    /// Gets the event timeout value
    ///
    /// Limits the amount of time that the client is willing
    /// to wait for a connection-related event, such as a
    /// connection or disconnection.
    ///
    /// Timeouts cause `TncError::CommandTimeout` errors.
    /// If an event times out, there is likely a serious
    /// problem with the ARDOP TNC or its connection.
    ///
    /// # Returns
    /// Current timeout value
    pub fn event_timeout(&self) -> &Duration {
        &self.event_timeout
    }

    /// Sets timeout for the control connection
    ///
    /// Limits the amount of time that the client is willing
    /// to wait for a connection-related event, such as a
    /// connection or disconnection.
    ///
    /// # Parameters
    /// - `timeout`: New event timeout value
    pub fn set_event_timeout(&mut self, timeout: Duration) {
        self.event_timeout = timeout;
    }

    /// Incoming events and data stream
    ///
    /// This stream emits both connection-relevant events and
    /// data received from remote peers.
    ///
    /// # Return
    /// Stream reference
    pub fn data_stream(&mut self) -> &mut (impl Stream<Item = DataEvent> + Unpin) {
        &mut self.data_stream
    }

    /// Outgoing data sink
    ///
    /// This sink accepts data for transmission over the air.
    /// Each FEC transmission must be sent in one call. ARQ
    /// data may be sent in a streaming manner.
    ///
    /// # Return
    /// Sink reference
    pub fn data_sink(&mut self) -> &mut (impl Sink<DataOut, SinkError = io::Error> + Unpin) {
        &mut self.data_stream
    }

    /// Send a command to the TNC and await the response
    ///
    /// A future which will send the given command and wait
    /// for a success or failure response. The waiting time
    /// is upper-bounded by AsyncTnc's `control_timeout()`
    /// value.
    ///
    /// # Parameters
    /// - `cmd`: The Command to send
    ///
    /// # Returns
    /// An empty on success or a `TncError`.
    pub async fn command<F>(&mut self, cmd: Command<F>) -> TncResult<()>
    where
        F: fmt::Display,
    {
        match execute_command(
            &mut self.control_out,
            &mut self.control_in_res,
            &self.control_timeout,
            cmd,
        )
        .await
        {
            Ok(cmdok) => {
                debug!(target: "tnc", "Command {} OK", cmdok.0);
                Ok(())
            }
            Err(e) => {
                warn!(target: "tnc", "Command failed: {}", &e);
                Err(e)
            }
        }
    }

    /// Dial a remote `target` peer
    ///
    /// When run, this future will
    ///
    /// 1. Wait for a clear channel
    /// 2. Make an outgoing `ARQCALL` to the designated callsign
    /// 3. Wait for a connection to either complete or fail
    ///
    /// # Parameters
    /// - `target`: Peer callsign, with optional `-SSID` portion
    /// - `bw`: ARQ bandwidth to use
    /// - `bw_forced`: If false, will potentially negotiate for a
    ///   *lower* bandwidth than `bw` with the remote peer. If
    ///   true, the connection will be made at `bw` rate---or not
    ///   at all.
    /// - `attempts`: Number of connection attempts to make
    ///   before giving up
    /// - `busy_timeout`: Wait this long, at maximum, for a clear
    ///   channel before giving up.
    ///
    /// # Return
    /// The outer result contains failures related to the local
    /// TNC connection.
    ///
    /// The inner result contains failures related to the RF
    /// connection. If the connection attempt succeeds, returns
    /// information about the connection. If the attempt fails,
    /// returns the failure reason.
    pub async fn connect<S>(
        &mut self,
        target: S,
        bw: u16,
        bw_forced: bool,
        attempts: u16,
        busy_timeout: Duration,
    ) -> TncResult<Result<ConnectionInfo, ConnectionFailedReason>>
    where
        S: Into<String>,
    {
        let target_string = target.into();

        // configure the ARQ mode
        self.command(command::protocolmode(ProtocolMode::ARQ))
            .await?;
        self.command(command::arqbw(bw, bw_forced)).await?;

        // wait for clear air, but give up after busy_timeout
        info!(target:"tnc", "Connecting to {}: waiting for clear channel...", &target_string);
        match Timed::platform_new(self.busy_mutex.lock(), busy_timeout).await {
            Err(_e) => {
                info!(target:"tnc",
                      "Connection to {} failed: {}",
                      &target_string,
                      &ConnectionFailedReason::Busy);
                return Ok(Err(ConnectionFailedReason::Busy));
            }
            Ok(_inner) => { /* no-op */ }
        }

        // dial
        //
        // success here merely indicates that a connect request is
        // in-flight
        info!(target:"tnc", "Connecting to {}: dialing at {} Hz BW...", &target_string, bw);
        match self
            .command(command::arqcall(target_string.clone(), attempts))
            .await
        {
            Ok(()) => { /* no-op */ }
            Err(e) => {
                error!(target:"tnc",
                      "Connection to {} failed: TNC rejected request: {}.",
                      &target_string,
                      &e);
                return Err(e);
            }
        }

        // wait for success or failure of the connection
        loop {
            match self.next_state_change().await? {
                ConnectionStateChange::Connected(info) => {
                    info!(target:"tnc", "CONNECTED {}", &info);
                    return Ok(Ok(info));
                }
                ConnectionStateChange::Failed(fail) => {
                    info!(target:"tnc",
                          "Connection to {} failed: {}",
                          &target_string,
                          &fail);
                    return Ok(Err(fail));
                }
                ConnectionStateChange::Closed => {
                    info!(target:"tnc",
                          "Connection to {} failed: not connected",
                          &target_string);
                    return Ok(Err(ConnectionFailedReason::NoAnswer));
                }
                _ => { /* ignore */ }
            }
        }
    }

    /// Listen for incoming connections
    ///
    /// When run, this future will wait for the TNC to accept
    /// an incoming connection to `MYCALL` or one of `MYAUX`.
    /// When a connection is accepted, the future will resolve
    /// to a `ConnectionInfo`.
    ///
    /// # Parameters
    /// - `bw`: ARQ bandwidth to use
    /// - `bw_forced`: If false, will potentially negotiate for a
    ///   *lower* bandwidth than `bw` with the remote peer. If
    ///   true, the connection will be made at `bw` rate---or not
    ///   at all.
    ///
    /// # Return
    /// The outer result contains failures related to the local
    /// TNC connection.
    ///
    /// The inner result contains failures related to the RF
    /// connection. At present, these are all consumed internally,
    /// but errors might be added in the future.
    pub async fn listen(
        &mut self,
        bw: u16,
        bw_forced: bool,
    ) -> TncResult<Result<ConnectionInfo, ConnectionFailedReason>> {
        // configure the ARQ mode and start listening
        self.command(command::protocolmode(ProtocolMode::ARQ))
            .await?;
        self.command(command::arqbw(bw, bw_forced)).await?;
        self.command(command::listen(true)).await?;

        info!(target:"tnc",
              "Listening for {} at {} Hz...",
              self.mycall(), bw);

        // wait until we connect
        loop {
            match self.next_state_change_timeout(Duration::from_secs(0)).await {
                Err(_timeout) => break,
                Ok(ConnectionStateChange::Connected(info)) => {
                    info!(target:"tnc", "CONNECTED {}", &info);
                    self.command(command::listen(false)).await?;
                    return Ok(Ok(info));
                }
                Ok(ConnectionStateChange::Failed(fail)) => {
                    info!(target:"tnc", "Incoming connection failed: {}", fail);
                }
                Ok(ConnectionStateChange::Closed) => {
                    info!(target:"tnc",
                          "Incoming connection failed: not connected");
                }
                _ => continue,
            }
        }

        // timed out
        self.command(command::listen(false)).await?;
        Ok(Err(ConnectionFailedReason::NoAnswer))
    }

    /// Query TNC version
    ///
    /// Queries the ARDOP TNC software for its version number.
    /// The format of the version number is unspecified and may
    /// be empty.
    ///
    /// # Return
    /// Version string, or an error if the version string could
    /// not be retrieved.
    pub async fn version(&mut self) -> TncResult<String> {
        let cmd = command::version();
        let vers = execute_command(
            &mut self.control_out,
            &mut self.control_in_res,
            &self.control_timeout,
            cmd,
        )
        .await?;
        match vers.1 {
            None => Ok("".to_owned()),
            Some(v) => Ok(v),
        }
    }

    /// Obtain spawner assigned at construction time
    ///
    /// # Return
    /// A reference to the spawner provided to the `AsyncTnc`
    /// at construction time. This spawner may be used to run
    /// async tasks.
    pub fn spawner(&mut self) -> &mut P {
        &mut self.spawner
    }

    // Initialize the ARDOP TNC
    async fn initialize(&mut self) -> TncResult<()> {
        self.data_stream.state_mut().reset();
        self.command(command::initialize()).await?;
        self.command(command::listen(false)).await?;
        self.command(command::protocolmode(ProtocolMode::FEC))
            .await?;
        self.command(command::mycall(self.mycall().clone())).await?;
        Ok(())
    }

    // wait for a connection state change
    async fn next_state_change(&mut self) -> TncResult<ConnectionStateChange> {
        self.next_state_change_timeout(self.event_timeout.clone())
            .await
    }

    // wait for a connection state change (specified timeout, zero for infinite)
    async fn next_state_change_timeout(
        &mut self,
        timeout: Duration,
    ) -> TncResult<ConnectionStateChange> {
        loop {
            let res = if timeout == Duration::from_secs(0) {
                self.data_stream.next().await
            } else {
                Timed::platform_new(self.data_stream.next(), timeout.clone()).await?
            };
            match res {
                None => return Err(connection_reset_err()),
                Some(DataEvent::Event(event)) => return Ok(event),
                Some(_data) => { /* consume it */ }
            }
        }
    }
}

impl<I, P> Unpin for AsyncTnc<I, P>
where
    I: AsyncRead + AsyncWrite + Unpin + Send + Sync,
    P: SpawnExt,
{
}

// Future which executes a command on the TNC
//
// Sends a command to the TNC and awaits the result.
// The caller will wait no longer than `timeout`.
async fn execute_command<'d, W, R, F>(
    outp: &'d mut W,
    inp: &'d mut R,
    timeout: &'d Duration,
    cmd: Command<F>,
) -> TncResult<CommandOk>
where
    W: Sink<String> + Unpin,
    R: Stream<Item = CommandResult> + Unpin,
    F: fmt::Display,
{
    let send_raw = cmd.to_string();

    // send
    let _ = Timed::platform_new(outp.send(send_raw), timeout.clone()).await?;

    match Timed::platform_new(inp.next(), timeout.clone()).await? {
        // lost connection
        None => Err(connection_reset_err()),
        // TNC FAULT means our command has failed
        Some(Err(badcmd)) => Err(TncError::CommandFailed(badcmd)),
        Some(Ok((in_id, msg))) => {
            if in_id == *cmd.command_id() {
                // success
                Ok((in_id, msg))
            } else {
                // success, but this isn't the command we are looking for
                Err(TncError::CommandResponseInvalid(
                    cmd.command_id().clone(),
                    in_id,
                ))
            }
        }
    }
}

fn connection_reset_err() -> TncError {
    TncError::IoError(io::Error::new(
        io::ErrorKind::ConnectionReset,
        "Lost connection to ARDOP TNC",
    ))
}

#[cfg(test)]
mod test {
    use super::*;

    use std::io::Cursor;

    use futures::executor;
    use futures::executor::LocalPool;
    use futures::sink;
    use futures::stream;

    use crate::protocol::constants::CommandID;

    #[test]
    fn test_execute_command_good_response() {
        let cmd_out = command::listen(true);
        let res_in: Vec<CommandResult> = vec![Ok((CommandID::LISTEN, None))];

        let mut sink_out = sink::drain();
        let mut stream_in = stream::iter(res_in.into_iter());
        let timeout = Duration::from_secs(10);

        let res = executor::block_on(execute_command(
            &mut sink_out,
            &mut stream_in,
            &timeout,
            cmd_out,
        ));
        match res {
            Ok((CommandID::LISTEN, None)) => assert!(true),
            _ => assert!(false),
        }
    }

    #[test]
    fn test_execute_command_bad_response() {
        let cmd_out = command::listen(true);
        let res_in: Vec<CommandResult> = vec![Ok((CommandID::VERSION, None))];

        let mut sink_out = sink::drain();
        let mut stream_in = stream::iter(res_in.into_iter());
        let timeout = Duration::from_secs(10);

        let res = executor::block_on(execute_command(
            &mut sink_out,
            &mut stream_in,
            &timeout,
            cmd_out,
        ));
        match res {
            Err(TncError::CommandResponseInvalid(CommandID::LISTEN, CommandID::VERSION)) => {
                assert!(true)
            }
            _ => assert!(false),
        }
    }

    #[test]
    fn test_execute_command_eof() {
        let cmd_out = command::listen(true);
        let res_in: Vec<CommandResult> = vec![];

        let mut sink_out = sink::drain();
        let mut stream_in = stream::iter(res_in.into_iter());
        let timeout = Duration::from_secs(10);

        let res = executor::block_on(execute_command(
            &mut sink_out,
            &mut stream_in,
            &timeout,
            cmd_out,
        ));
        match res {
            Err(TncError::IoError(e)) => assert_eq!(e.kind(), io::ErrorKind::ConnectionReset),
            _ => assert!(false),
        }
    }

    #[test]
    fn test_execute_command_timeout() {
        let cmd_out = command::listen(true);

        let mut sink_out = sink::drain();
        let mut stream_in = stream::once(futures::future::empty());
        let timeout = Duration::from_nanos(1);

        let res = executor::block_on(execute_command(
            &mut sink_out,
            &mut stream_in,
            &timeout,
            cmd_out,
        ));
        match res {
            Err(TncError::CommandTimeout) => assert!(true),
            _ => assert!(false),
        }
    }

    #[test]
    fn test_streams() {
        let mut pool = LocalPool::new();
        let stream_ctrl = Cursor::new(b"BUSY FALSE\rREJECTEDBW\r".to_vec());
        let stream_data = Cursor::new(b"ARQ\x00\x05HELLO".to_vec());

        let mut tnc = AsyncTnc::new_from_streams(
            pool.spawner(),
            stream_ctrl,
            stream_data,
            "W1AW",
            Duration::from_secs(0),
        );

        pool.run_until(async {
            match tnc.data_stream().next().await {
                Some(DataEvent::Data(_d)) => assert!(true),
                _ => assert!(false),
            }

            match tnc.data_stream().next().await {
                Some(DataEvent::Event(ConnectionStateChange::Failed(
                    ConnectionFailedReason::IncompatibleBandwidth,
                ))) => assert!(true),
                _ => assert!(false),
            }

            assert!(tnc.data_stream().next().await.is_none());
        });
    }

}
