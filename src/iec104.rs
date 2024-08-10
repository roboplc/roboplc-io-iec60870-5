use core::fmt;
use std::{collections::BTreeSet, io::Cursor, net::ToSocketAddrs, sync::Arc, time::Duration};

static PUSH_COTS: Lazy<BTreeSet<COT>> =
    Lazy::new(|| BTreeSet::from_iter([COT::Cyclic, COT::Background, COT::Spontan, COT::Init]));

use iec60870_5::{
    telegram104::{ChatSequenceCounter, Telegram104, Telegram104_S},
    types::COT,
};
use once_cell::sync::Lazy;
use roboplc::{comm::ConnectionHandler, locking::Mutex};
use roboplc::{
    comm::{CommReader, Stream, Timeouts},
    policy_channel::{self, Receiver, Sender},
    DataDeliveryPolicy, Result,
};
use rtsc::{cell::DataCell, time::interval};
use tracing::{debug, error, trace, warn};

/// Ping kind
#[derive(Copy, Clone, Default, Eq, PartialEq, Debug)]
pub enum PingKind {
    /// Re-connect socket if dropped
    Connect,
    #[default]
    /// Send test frame (U-frame)
    Test,
    /// Send ack frame (S-frame)
    Ack,
}

impl fmt::Display for PingKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PingKind::Connect => write!(f, "socket connect"),
            PingKind::Test => write!(f, "IEC 60870-5 104 test U-frame"),
            PingKind::Ack => write!(f, "IEC 60870-5 104 ack S-frame"),
        }
    }
}

/// Pinger worker
#[derive(Clone)]
pub struct Pinger {
    inner: Arc<Client104Inner>,
    kind: PingKind,
    interval: Duration,
}

impl Pinger {
    /// Run the pinger worker
    pub fn run(&self) {
        trace!(?self.kind, "pinger started");
        for _ in interval(self.interval) {
            let result = match self.kind {
                PingKind::Connect => self.inner.client.connect(),
                PingKind::Test => {
                    let frame = Telegram104::new_test();
                    self.inner.send(frame)
                }
                PingKind::Ack => {
                    let frame = Telegram104_S::new();
                    self.inner.send(frame.into())
                }
            };
            if let Err(error) = result {
                error!(%error, kind=%self.kind, "remote ping error");
            }
        }
    }
}

/// IEC 60870-5 104 client
#[derive(Clone)]
pub struct Client {
    inner: Arc<Client104Inner>,
}

impl Client {
    /// Create a new client
    #[allow(clippy::missing_panics_doc)]
    pub fn new<A: ToSocketAddrs + fmt::Debug>(
        addr: A,
        timeouts: Timeouts,
        reader_queue_size: usize,
    ) -> Result<(Self, Reader)> {
        // make sure lazy is initialized
        assert!(!PUSH_COTS.is_empty());
        let (inner, reader) = Client104Inner::new(addr, timeouts, reader_queue_size)?;
        Ok((
            Self {
                inner: Arc::new(inner),
            },
            reader,
        ))
    }
    /// Need to be called periodically to accept server pushes if no keep-alive mechanism is
    /// implemented. Does not need to be called if keep-alive is present.
    pub fn connect(&self) -> Result<()> {
        self.inner.client.connect()
    }
    /// Send a frame
    #[inline]
    pub fn send(&self, frame: Telegram104) -> Result<()> {
        self.inner.send(frame)
    }
    /// Send a command frame and wait for the response
    #[inline]
    pub fn command(&self, frame: Telegram104) -> Result<Telegram104> {
        self.inner.command(frame)
    }
    /// Create a pinger worker object
    pub fn pinger(&self, kind: PingKind, interval: Duration) -> Pinger {
        Pinger {
            inner: self.inner.clone(),
            kind,
            interval,
        }
    }
}

type CommandResponseTx = Arc<Mutex<Option<DataCell<Telegram104>>>>;

struct Client104Inner {
    client: roboplc::comm::Client,
    connection_handler: IecConnectionHandler,
    timeouts: Timeouts,
    command_response_tx: CommandResponseTx,
    command_lock: Mutex<()>,
}

#[derive(Clone, Default)]
struct IecConnectionHandler {
    chat_seq: ChatSequenceCounter,
    chat_seq_lock: Arc<Mutex<()>>,
}

impl ConnectionHandler for IecConnectionHandler {
    fn on_connect(
        &self,
        stream: &mut dyn Stream,
    ) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.chat_seq.reset();
        let mut req = Cursor::new(Vec::new());
        Telegram104::new_start_dt().write(&mut req)?;
        stream.write_all(&req.into_inner())?;
        let reply = Telegram104::read(stream)?;
        let Telegram104::U(r) = reply else {
            return Err("unexpected reply".into());
        };
        if !r.is_start_dt() || !r.is_con() {
            return Err("chat reply is not start_dt confirmation".into());
        }
        Ok(())
    }
}

impl Client104Inner {
    pub fn new<A: ToSocketAddrs + fmt::Debug>(
        addr: A,
        timeouts: Timeouts,
        reader_queue_size: usize,
    ) -> Result<(Self, Reader)> {
        let connection_handler = IecConnectionHandler::default();

        let (client, reader_rx) = roboplc::comm::tcp::connect_with_options(
            addr,
            roboplc::comm::ConnectionOptions::new(timeouts.connect)
                .with_reader()
                .connection_handler(connection_handler.clone())
                .timeouts(timeouts.clone()),
        )?;
        let reader_rx = reader_rx.expect("reader_rx");
        let (restart_tx, restart_rx) = policy_channel::bounded(1);
        let (telegram_tx, telegram_rx) = rtsc::channel::bounded(reader_queue_size);

        let command_response_tx: CommandResponseTx = <_>::default();

        let reader = Reader {
            client: client.clone(),
            reader_rx,
            restart_rx,
            restart_tx,
            telegram_rx,
            telegram_tx,
            command_response_tx: command_response_tx.clone(),
            connection_handler: connection_handler.clone(),
        };

        Ok((
            Self {
                client,
                connection_handler,
                timeouts,
                command_response_tx,
                command_lock: Mutex::new(()),
            },
            reader,
        ))
    }

    pub fn send(&self, mut frame: Telegram104) -> Result<()> {
        let _chat_seq_lock = self
            .connection_handler
            .chat_seq_lock
            .try_lock_for(self.timeouts.write);
        // lock session for I and S frames to prevent reconnects and chat sequence errors
        let _sess = if matches!(frame, Telegram104::I(_) | Telegram104::S(_)) {
            Some(self.client.lock_session()?)
        } else {
            None
        };
        let mut req = Cursor::new(Vec::new());
        frame.chat_sequence_apply_outgoing(&self.connection_handler.chat_seq);
        frame.write(&mut req).map_err(roboplc::Error::io)?;
        self.client.write(&req.into_inner())?;
        Ok(())
    }

    pub fn command(&self, frame: Telegram104) -> Result<Telegram104> {
        let _lock = self.command_lock.try_lock_for(self.timeouts.write);
        let cell = DataCell::new();
        self.command_response_tx.lock().replace(cell.clone());
        if let Err(e) = self.send(frame) {
            if let Some(d) = self.command_response_tx.lock().take() {
                d.close();
            }
            return Err(e);
        }
        cell.get_timeout(self.timeouts.write).map_err(Into::into)
    }
}

/// Received every time when the reader has been restarted.
#[derive(Default, Copy, Clone)]
pub struct RestartEvent {}

impl DataDeliveryPolicy for RestartEvent {
    fn delivery_policy(&self) -> roboplc::prelude::DeliveryPolicy {
        roboplc::prelude::DeliveryPolicy::Single
    }
}

/// IEC 60870-5 104 reader
pub struct Reader {
    client: roboplc::comm::Client,
    reader_rx: Receiver<CommReader>,
    restart_rx: Receiver<RestartEvent>,
    restart_tx: Sender<RestartEvent>,
    telegram_rx: roboplc::channel::Receiver<Telegram104>,
    telegram_tx: roboplc::channel::Sender<Telegram104>,
    command_response_tx: CommandResponseTx,
    connection_handler: IecConnectionHandler,
}

impl Reader {
    /// The method is required to be started in a separate thread.
    ///
    /// # Panics
    ///
    /// Should not panic
    pub fn run(&self) {
        let mut first_start = true;
        while let Ok(reader) = self.reader_rx.recv() {
            let session_id = self.client.session_id();
            self.restart_tx
                .send(RestartEvent {})
                .expect("never disconnects");
            if first_start {
                first_start = false;
            } else {
                warn!(session_id, "IEC 60870-5 104 reader loop restarted");
            }
            trace!(session_id, "spawning reader");
            self.run_inner(reader);
            // reconnect the client in case it has not been done yet
            if session_id == self.client.session_id() {
                debug!("reader asked the client to reconnect");
                self.client.reconnect();
            }
        }
    }

    /// Gets a channel receiver for restart events. The events can be processed later manually,
    /// e.g. to restore notifications or handles.
    ///
    /// The restart beacon has got a delivery policy `Single` so the event is always delivered in a
    /// single copy, no matter how many restarts happened.
    ///
    /// NOTE: it is physically impossible to be 100% was there a network issue or the server has been
    /// restarted. In production networks, consider the network issue is the less likely case.
    #[allow(dead_code)]
    // the function is being tested for stability
    pub fn get_restart_event_receiver(&self) -> Receiver<RestartEvent> {
        self.restart_rx.clone()
    }

    /// Gets a channel receiver for frames.
    pub fn get_telegram_receiver(&self) -> roboplc::channel::Receiver<Telegram104> {
        self.telegram_rx.clone()
    }

    fn run_inner(&self, mut reader: CommReader) {
        let mut socket = reader.take().expect("can not get reader socket");
        loop {
            let telegram = match Telegram104::read(&mut socket) {
                Ok(telegram) => telegram,
                Err(error) => {
                    error!(%error, "IEC 60870-5 104 reader error");
                    break;
                }
            };
            {
                let _chat_seq_lock = self.connection_handler.chat_seq_lock.lock();
                if let Err(e) =
                    telegram.chat_sequence_validate_incoming(&self.connection_handler.chat_seq)
                {
                    error!(%e, "IEC 60870-5 104 reader chat sequence error");
                    break;
                }
            }
            if let Telegram104::I(ref i) = telegram {
                if !PUSH_COTS.contains(&i.cot()) {
                    if let Some(ref command_response_tx) = self.command_response_tx.lock().take() {
                        if !command_response_tx.is_closed() {
                            command_response_tx.set(telegram);
                        }
                        continue;
                    }
                }
            }
            if self.telegram_tx.send(telegram).is_err() {
                error!("IEC 60870-5 104 reader telegram_tx failed");
                break;
            }
        }
    }
}
