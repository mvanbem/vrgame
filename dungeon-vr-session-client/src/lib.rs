use std::future::pending;
use std::time::Duration;

use dungeon_vr_connection_client::{
    ConnectionState, Event as ConnectionEvent, Request as ConnectionRequest,
};
use dungeon_vr_session_shared::net_game::{NetGame, NetGameFullCodec};
use dungeon_vr_session_shared::packet::game_state_packet::GameStatePacket;
use dungeon_vr_session_shared::packet::ping_packet::PingPacket;
use dungeon_vr_session_shared::packet::pong_packet::PongPacket;
use dungeon_vr_session_shared::packet::voice_packet::VoicePacket;
use dungeon_vr_session_shared::packet::Packet;
use dungeon_vr_session_shared::time::ClientEpoch;
use dungeon_vr_stream_codec::{ExternalStreamCodec, StreamCodec};
use futures::FutureExt;
use tokio::select;
use tokio::sync::mpsc;
use tokio::time::{interval, Interval};

const EVENT_BUFFER_SIZE: usize = 256;
const REQUEST_BUFFER_SIZE: usize = 256;

pub struct SessionClient {
    cancel_guard: cancel::Guard,
    events: mpsc::Receiver<Event>,
    requests: mpsc::Sender<Request>,
}

enum InternalEvent {
    Connection(Option<ConnectionEvent>),
    Request(Option<Request>),
    TimeSync,
}

impl SessionClient {
    pub fn new(
        connection_requests: mpsc::Sender<ConnectionRequest>,
        connection_events: mpsc::Receiver<ConnectionEvent>,
    ) -> Self {
        let cancel_token = cancel::Token::new();
        let (event_tx, event_rx) = mpsc::channel(EVENT_BUFFER_SIZE);
        let (request_tx, request_rx) = mpsc::channel(REQUEST_BUFFER_SIZE);
        tokio::spawn(
            InnerClient::new(
                cancel_token.clone(),
                connection_requests,
                connection_events,
                event_tx,
                request_rx,
            )
            .run(),
        );
        Self {
            cancel_guard: cancel_token.guard(),
            events: event_rx,
            requests: request_tx,
        }
    }

    pub fn try_recv_event(&mut self) -> Option<Event> {
        self.events.try_recv().ok()
    }

    pub async fn recv_event(&mut self) -> Event {
        self.events.recv().await.unwrap()
    }

    pub fn split(self) -> (cancel::Guard, mpsc::Receiver<Event>, mpsc::Sender<Request>) {
        (self.cancel_guard, self.events, self.requests)
    }
}

struct InnerClient {
    cancel_token: cancel::Token,
    connection_requests: mpsc::Sender<ConnectionRequest>,
    connection_events: mpsc::Receiver<ConnectionEvent>,
    events: mpsc::Sender<Event>,
    requests: mpsc::Receiver<Request>,
    epoch: ClientEpoch,
    time_sync: Option<Interval>,
    client_time_to_server_time: Option<f64>,
}

pub enum Event {
    GameState(NetGame),
    Voice(Vec<u8>),
}

pub enum Request {
    SendVoice(Vec<u8>),
}

impl InnerClient {
    fn new(
        cancel_token: cancel::Token,
        connection_requests: mpsc::Sender<ConnectionRequest>,
        connection_events: mpsc::Receiver<ConnectionEvent>,
        events: mpsc::Sender<Event>,
        requests: mpsc::Receiver<Request>,
    ) -> Self {
        Self {
            cancel_token: cancel_token.clone(),
            connection_requests,
            connection_events,
            events,
            requests,
            epoch: ClientEpoch::new(),
            time_sync: None,
            client_time_to_server_time: None,
        }
    }

    async fn run(mut self) {
        while !self.cancel_token.is_cancelled() {
            let time_sync = match self.time_sync.as_mut() {
                Some(time_sync) => time_sync.tick().left_future(),
                None => pending().right_future(),
            };

            let event = select! {
                biased;

                _ = self.cancel_token.cancelled() => break,

                event = self.connection_events.recv() => InternalEvent::Connection(event),

                request = self.requests.recv() => InternalEvent::Request(request),

                _ = time_sync => InternalEvent::TimeSync,

            };

            match event {
                InternalEvent::Connection(event) => self.handle_connection_event(event).await,
                InternalEvent::Request(request) => self.handle_request(request).await,
                InternalEvent::TimeSync => self.handle_time_sync().await,
            }
        }

        // Wait for the connection to drop.
        loop {
            match self.connection_events.recv().await {
                Some(ConnectionEvent::Dropped) => break,
                Some(_) => (),
                None => {
                    log::error!("Connection closed its event channel before signaling Dropped");
                }
            }
        }
    }

    async fn handle_connection_event(&mut self, event: Option<ConnectionEvent>) {
        match event.unwrap() {
            ConnectionEvent::State(state) => self.handle_connection_state(state),
            ConnectionEvent::GameData(data) => self.handle_connection_game_data(data).await,
            ConnectionEvent::Dropped => self.handle_connection_dropped(),
        }
    }

    fn handle_connection_state(&mut self, state: ConnectionState) {
        match state {
            ConnectionState::Disconnected => {
                self.cancel_token.cancel();
            }
            ConnectionState::Connecting => (),
            ConnectionState::Responding => (),
            ConnectionState::Connected => {
                self.time_sync = Some(interval(Duration::from_secs(10)));
            }
        }
    }

    async fn handle_connection_game_data(&mut self, data: Vec<u8>) {
        let mut r = data.as_slice();
        let packet = match Packet::read_from(&mut r) {
            Ok(packet) => packet,
            Err(e) => {
                log::error!("Error decoding game data packet: {e}");
                return;
            }
        };
        if !r.is_empty() {
            log::error!(
                "Dropping {:?} game data packet: {} unexpected trailing byte(s)",
                packet.kind(),
                r.len(),
            );
            return;
        }
        match packet {
            Packet::Pong(packet) => self.handle_pong_packet(packet),
            Packet::GameState(packet) => self.handle_game_state_packet(packet).await,
            Packet::Voice(packet) => self.handle_voice_packet(packet).await,
            _ => {
                log::error!("Unexpected game data packet: {:?}", packet.kind());
            }
        }
    }

    fn handle_pong_packet(&mut self, packet: PongPacket) {
        let now = self.epoch.now();
        let elapsed = now - packet.client_time;
        let midpoint = packet.client_time + elapsed / 2;
        let client_time_to_server_time = packet.server_time.to_micros_since_epoch() as f64
            - midpoint.to_micros_since_epoch() as f64;
        self.client_time_to_server_time = Some(match self.client_time_to_server_time {
            Some(prev) => {
                let next = 0.9 * prev + 0.1 * client_time_to_server_time;
                log::debug!("Time sync: Adjusting offset from {prev} to {next}");
                next
            }
            None => {
                log::debug!("Time sync: Initial offset is {client_time_to_server_time}");
                client_time_to_server_time
            }
        });
    }

    async fn handle_game_state_packet(&mut self, packet: GameStatePacket) {
        let mut r = packet.serialized_game_state.as_slice();
        let game_state = match NetGameFullCodec::read_from_ext(&mut r) {
            Ok(game_state) => game_state,
            Err(e) => {
                log::error!("Error decoding game state: {e}");
                return;
            }
        };
        if !r.is_empty() {
            log::error!(
                "Dropping game state: {} unexpected trailing byte(s)",
                r.len(),
            );
            return;
        }
        let _ = self.events.send(Event::GameState(game_state)).await;
    }

    async fn handle_voice_packet(&mut self, packet: VoicePacket) {
        let _ = self.events.send(Event::Voice(packet.data)).await;
    }

    fn handle_connection_dropped(&mut self) {
        todo!()
    }

    async fn handle_request(&mut self, request: Option<Request>) {
        match request.unwrap() {
            Request::SendVoice(data) => {
                send_packet(
                    &self.connection_requests,
                    Packet::Voice(VoicePacket { data }),
                )
                .await;
            }
        }
    }

    async fn handle_time_sync(&mut self) {
        log::debug!("Requesting time sync");
        send_packet(
            &self.connection_requests,
            Packet::Ping(PingPacket {
                client_time: self.epoch.now(),
            }),
        )
        .await;
    }
}

async fn send_packet(requests: &mpsc::Sender<ConnectionRequest>, packet: Packet) {
    let mut data = Vec::new();
    packet.write_to(&mut data).unwrap();
    let _ = requests.send(ConnectionRequest::SendGameData(data)).await;
}