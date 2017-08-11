use model::*;
use error::Error;

use tokio_core::reactor::Handle;
use futures::sync::mpsc::{self};

use super::imports::*;
use super::state_update_sink::*;

pub mod public {
    pub use super::{
        VoiceEvent,
        SpeakingUpdate,
        VoicePacket,
        VoiceMessage,
        VoiceFrame,
        VoiceControlChannel,
        VoiceChannel,
    };
}

/// Events received over a voice channel
pub enum VoiceEvent {
    SpeakingUpdate(SpeakingUpdate),
    VoicePacket(VoicePacket),
    Disconnected // TODO disconnect reason
}

/// Sent when a user's currently-speaking state has updated.
///
/// This event is the only way to know the `ssrc` to `user_id` mapping, but is unreliable and
/// only a hint for when users are actually speaking, due both to latency differences and that
/// it is possible for a user to leave `speaking` true even when they are not sending audio.
pub struct SpeakingUpdate {
    pub ssrc: u32,
    pub user_id: UserId,
    pub speaking: bool
}

/// Sent when a voice packet is received.
///
/// The sequence number increases by one per packet sent, and can be used to reorder packets
/// if they have been received out of order. The timestamp increases at 48000Hz (typically by
/// 960 per 20ms frame). If `stereo` is true, the length of the `data` slice is doubled and
/// samples have been interleaved. The typical length of `data` is 960 or 1920 for a 20ms frame,
/// but may be larger or smaller in some situations.
pub struct VoicePacket {
    pub ssrc: u32,
    pub sequence: u16,
    pub timestamp: u32,
    pub stereo: bool,
    pub data: Vec<u16>
}

/// Events sent _to_ a voice channel.
pub enum VoiceMessage {
    /// A voice frame with audio data
    VoiceFrame(VoiceFrame),
    /// A frame of silence. This will keep the channel open.
    SilenceFrame,
    /// Sets whether this connection is muted. This is a cosmetic indication; you must also ensure
    /// that you are sending silence frames in the meantime.
    SetMute(bool),
    /// Sets whether this connection is deafened.
    SetDeaf(bool),
    /// Send to request a graceful shutdown of the voice connection
    Disconnect,
    /// Send to request a channel change
    ChangeChannel(ChannelId)
}

pub struct VoiceFrame {
    /// Indicates whether the stream is stereo or not. Changing this value between frames results
    /// in expensive reinitialization of the audio codec and should be avoided.
    is_stereo: bool,

    buffer: Vec<u16>
}

#[derive(Debug, Clone)]
pub struct VoiceChannelState {
    server_id: ServerId,
    endpoint: Option<String>,
    user_id: Option<UserId>,
    session_id: Option<String>,
    voice_token: Option<String>,
    current_channel: Option<ChannelId>
}

pub struct VoiceControlChannel {
    state_rx: mpsc::Receiver<VoiceChannelState>,
    mesg_tx: mpsc::Sender<::serde_json::Value>
}

pub struct VoiceChannel {}

impl VoiceControlChannel {
    pub fn connect(
        self, handle: &Handle,
        channel_id: ChannelId,
        initially_muted: bool, initially_deaf: bool
    ) -> VoiceChannel {
        panic!();
    }
}


#[derive(Clone, Debug)]
pub enum VoiceChannelNotice {
    VoiceServerUpdate {
        server_id: Option<ServerId>,
        channel_id: Option<ChannelId>,
        endpoint: Option<String>,
        token: String,
    },
    SessionChange {
        session_id: String,
    },
    Init {
        user_id: UserId
    },
    Resumed
}

pub fn new_handle(server_id: ServerId, mesg_tx: mpsc::Sender<::serde_json::Value>)
    -> (Box<Sink<SinkItem=VoiceChannelNotice, SinkError=Error>>, VoiceControlChannel)
{
    let (state_tx, state_rx) = mpsc::channel(1);

    // Building the Connection->VoiceChannel path. We'll want to set up a StateUpdateSink to
    // summarize voice state changes in case we get backed up.
    let channel_tell = state_tx.sink_map_err(|_| Error::Other("Detached from discord connection"));

    let mut state = VoiceChannelState {
        server_id: server_id,
        endpoint: None,
        user_id: None,
        session_id: None,
        voice_token: None,
        current_channel: None,
    };
    state.server_id = server_id;

    let channel_tell = StateUpdateSink::new(channel_tell, move |event: VoiceChannelNotice| {
        match event {
            VoiceChannelNotice::VoiceServerUpdate{ server_id, channel_id, endpoint, token } => {
                state.current_channel = channel_id.or(state.current_channel);
                state.endpoint = endpoint;
                state.voice_token = Some(token);
            },
            VoiceChannelNotice::SessionChange{session_id} => {
                // Our session was reset, so we need to reconnect.
                state.current_channel = None;
                state.endpoint = None;
                state.voice_token = None;
                state.session_id = Some(session_id);
            },
            VoiceChannelNotice::Init{user_id} => {
                state.user_id = Some(user_id);
            },
            VoiceChannelNotice::Resumed => {
                // no change, but the poke will get us to resend any outbound requests
            }
        }

        Ok(state.clone())
    });

    let channel_tell = Box::new(channel_tell);

    let voice_ctl = VoiceControlChannel { state_rx: state_rx, mesg_tx: mesg_tx };

    return (channel_tell, voice_ctl);
}