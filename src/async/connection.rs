use super::imports::*;
use super::single_conn::{SingleConnection, start_connect};
use error::Error;
use std::sync::{Arc,Mutex};
use Discord;
use model::*;
use serde_json;
use std::collections::HashMap;
use futures::sync::mpsc;
use super::voice_internal::VoiceChannelNotice;

pub struct Connection {
    upstream: Reconnector,
    voice_cmd_rx: Option<mpsc::Receiver<::serde_json::Value>>,
    voice_cmd_tx: Option<mpsc::Sender<::serde_json::Value>>,
    pending_voice_cmd: Option<::serde_json::Value>,
    voice_channels: HashMap<ServerId, Box<Sink<SinkItem=VoiceChannelNotice, SinkError=Error>>>,
    user_id: Option<UserId>,
    session_id: Option<String>,
}

impl Stream for Connection {
    type Item = Event;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        // Make progress on pushing things to voice control channels first
        // We do this here, rather than in poll_complete, because sending to these channels is
        // triggered by _incoming_ messages in the context of poll().
        // There's no guarantee that the caller will invoke our poll_complete, well, ever.
        for mut server_handle in self.voice_channels.values_mut() {
            server_handle.poll_complete()?;
        }

        let result = self.upstream.poll();

        match &result {
            &Ok(Async::Ready(Some(Event::Ready(ref readyev)))) => {
                let new_user_id = Some(readyev.user.id);
                if new_user_id != self.user_id {
                    self.user_id = new_user_id;
                    self.voice_inform(&None, || VoiceChannelNotice::Init { user_id: new_user_id.unwrap() });
                }

                let new_session_id = Some(readyev.session_id.clone());
                if new_session_id != self.session_id {
                    self.session_id = new_session_id.clone();
                    self.voice_inform(&None, || VoiceChannelNotice::SessionChange
                        { session_id: new_session_id.unwrap() }
                    );
                }
            },
            &Ok(Async::Ready(Some(Event::Resumed { trace: _ }))) => {
                // Let the voice session know, if it's waiting for a VoiceServerUpdate it'll resend
                // the command in case it got dropped
                self.voice_inform(&None, || VoiceChannelNotice::Resumed);
            },
            &Ok(Async::Ready(Some(Event::VoiceServerUpdate {
                ref server_id, ref channel_id, ref endpoint, ref token
            }))) => {
                if server_id.is_some() {
                    self.voice_inform(server_id,
                                      || VoiceChannelNotice::VoiceServerUpdate {
                                          endpoint: endpoint.clone(),
                                          server_id: server_id.clone(),
                                          token: token.clone(),
                                          channel_id: channel_id.clone()
                                      }
                    );
                }
            },
            &Ok(Async::NotReady) => {
                // nothing to read? make sure we keep making progress on the send side as well.
                self.process_voice_commands()?;
            }
            _ => { /* ignore */ }
        }

        return result;
    }
}

impl Sink for Connection {
    type SinkItem = serde_json::Value;
    type SinkError = Error;

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        self.process_voice_commands()?;

        self.upstream.poll_complete()
    }

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        self.process_voice_commands()?;

        self.upstream.start_send(item)
    }

    fn close(&mut self) -> Poll<(), Self::SinkError> {
        // Kill all voice connections immediately.
        self.voice_cmd_rx = None;
        self.voice_cmd_tx = None;
        self.pending_voice_cmd = None;
        self.voice_channels.clear();

        self.upstream.close()
    }
}

impl Connection {
    fn voice_inform<T>(&mut self, server_id: &Option<ServerId>, message_provider: T)
        where T: FnOnce()->VoiceChannelNotice
    {
        if server_id.is_some() {
            if let Some(ref mut server_handle) = self.voice_channels.get_mut(&server_id.unwrap()) {
                server_handle.start_send(message_provider());
            };
        } else {
            let message = message_provider();
            for mut server_handle in self.voice_channels.values_mut() {
                server_handle.start_send(message.clone());
            }
        }
    }

    fn process_voice_commands(&mut self) -> Result<(), Error> {
        if let Some(ref mut rx) = self.voice_cmd_rx {
            if !self.pending_voice_cmd.is_some() {
                match rx.poll() {
                    Ok(Async::NotReady) => {},
                    Ok(Async::Ready(None)) | Err(_) => {
                        // Hmm... the tx end was lost. This should never happen.
                        panic!("Impossible: voice command channel was closed");
                    },
                    Ok(Async::Ready(cmd)) => self.pending_voice_cmd = cmd,
                }
            }
        }

        if let Some(cmd) = self.pending_voice_cmd.take() {
            match self.upstream.start_send(cmd)? {
                AsyncSink::NotReady(cmd) => self.pending_voice_cmd = Some(cmd),
                AsyncSink::Ready => {
                    // run another loop in case more messages are waiting.
                    // we must continue running until _something_ returns NotReady.
                    return self.process_voice_commands();
                }
            }
        } else {
            // voice_cmd_rx returned NotReady above. Let's just prod the underlying connection to
            // make some progress on sending.
            self.upstream.poll_complete()?;
        }

        Ok(())
    }

    pub fn connect(discord: Discord, shard_info: Option<[u8; 2]>, handle: &Handle) -> Self {
        let mut conn = Reconnector {
            handle: handle.clone(),
            session_info: Arc::new(Mutex::new(
                SessionInfo {
                    discord: Arc::new(discord),
                    gateway_url: None,
                    gateway_failures: 0,
                    session_id: None,
                    last_seq: 0,
                    shard_info: shard_info,
                    keepalive_interval: 0,
                    timer: ::tokio_timer::Timer::default()
                }
            )),
            state: ConnState::Closed()
        };

        conn._reconnect();

        return Connection {
            upstream: conn,
            voice_cmd_tx: None,
            voice_cmd_rx: None,
            pending_voice_cmd: None,
            voice_channels: HashMap::new(),
            session_id: None,
            user_id: None
        };
    }

    fn setup_tx_channel(&mut self) {
        if self.voice_cmd_tx.is_none() {
            let (tx, rx) = mpsc::channel(1);
            self.voice_cmd_rx = Some(rx);
            self.voice_cmd_tx = Some(tx);
        }
    }

    pub fn voice_handle(&mut self, server_id: ServerId) -> super::voice_internal::VoiceControlChannel {
        self.setup_tx_channel();

        use super::voice_internal::new_handle;
        let (mut sink, handle) = new_handle(server_id, self.voice_cmd_tx.clone().unwrap());

        // Replay initialization messages if needed
        if self.user_id.is_some() {
            // start_send here is guaranteed to not return NotReady as it contains a (boxed)
            // StateUpdateSink
            let _ = sink.start_send(VoiceChannelNotice::Init {user_id: self.user_id.unwrap()});

            if self.session_id.is_some() {
                let _ = sink.start_send(
                    VoiceChannelNotice::SessionChange {session_id: self.session_id.clone().unwrap()}
                );
            }
        }

        self.voice_channels.insert(server_id, sink);

        return handle;
    }
}

/// This stream+sink offers a stream of events to/from discord that will automatically reconnect.
/// Voice handling is performed at the Connection level.
pub struct Reconnector {
    handle: Handle,
    session_info: SessionInfoRef,
    state: ConnState
}

enum ConnState {
    Connecting(LocalFuture<SingleConnection, Error>),
    Active(SingleConnection),
    Closed()
}

impl Reconnector {


    // Start the connection process, from any context (ignoring closed state etc)
    fn _reconnect(&mut self) {
        println!("!!! RECONNECT");
        self.state = ConnState::Connecting(start_connect(&self.handle, self.session_info.clone()));
    }

    fn reconnect(&mut self) {
        if let ConnState::Closed() = self.state {
            return;
        }

        self._reconnect();

        // important: We need to poll at least once to make sure we'll be notified when this finishes
        let _ = self.connection();
    }

    fn connection(&mut self) -> Poll<&mut SingleConnection, Error> {
        match self.state {
            ConnState::Active(ref mut conn) => {
                return Ok(Async::Ready(conn));
            },
            ConnState::Connecting(_) => {
                return self.finish_connect();
            },
            ConnState::Closed() => {
                return Err(Error::Other("Attempted to use a closed connection"))
            }
        }
    }

    fn finish_connect(&mut self) -> Poll<&mut SingleConnection, Error> {
        // Move the future out of our state field so we can (potentially) replace it
        let mut state = ::std::mem::replace(&mut self.state, ConnState::Closed());
        match state {
            ConnState::Connecting(ref mut f) => {
                match f.poll() {
                    Ok(Async::Ready(conn)) => {
                        println!("conn ready");
                        // This overwrite would cause us problems without the little dance above
                        self.state = ConnState::Active(conn);
                        return self.connection();
                    },
                    Err(e) => {
                        // as would this reconnect
                        self.reconnect();
                        return Ok(Async::NotReady);
                    },
                    Ok(Async::NotReady) => {
                        /* fall through to release borrows on state */
                    },
                }
            }
            _ => unreachable!()
        }

        // If we fell through to here, it means the future wasn't ready, so we need to put it back
        // into the state field.
        self.state = state;

        Ok(Async::NotReady)
    }
}

impl Stream for Reconnector {
    type Item = Event;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        let mut need_reconnect = false;

        if let Ok(Async::Ready(ref mut conn)) = self.connection() {
            match conn.poll() {
                Err(_) | Ok(Async::Ready(None)) => {
                    need_reconnect = true;
                    // fall through to release borrows
                },
                success => {
                    return success;
                }
            }
        }

        if need_reconnect {
            self.reconnect();
        }

        Ok(Async::NotReady)
    }
}

impl Sink for Reconnector {
    type SinkItem = serde_json::Value;
    type SinkError = Error;


    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        use futures::*;

        let mut need_reconnect = false;

        if let Async::Ready(ref mut conn) = self.connection()? {
            match conn.poll_complete() {
                Err(_) => {
                    need_reconnect = true;
                    // fall through to drop borrows
                },
                success => return success
            }
        }

        if need_reconnect {
            self.reconnect();
        }

        Ok(Async::NotReady)
    }

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        match self.connection() {
            Err(e) => return Err(e),
            Ok(Async::NotReady) => return Ok(AsyncSink::NotReady(item)),
            Ok(Async::Ready(ref mut conn)) => {
                match conn.start_send(item) {
                    Err(_) => {
                        // fall out of match to drop borrows so we can reconnect
                        // note that we didn't get the item back, so it'll just be lost...
                    }
                    other => { return other; }
                }
            }
        };

        self.reconnect();

        // lie and say we sent the message, because the Err didn't hand it back :(
        return Ok(AsyncSink::Ready);
    }

    fn close(&mut self) -> Poll<(), Self::SinkError> {
        if let ConnState::Active(mut conn) = ::std::mem::replace(&mut self.state, ConnState::Closed()) {
            conn.close();
        }

        Ok(Async::Ready(()))
    }
}