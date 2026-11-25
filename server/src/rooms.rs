use std::{
    collections::{HashMap, hash_map::Entry},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use axum::extract::ws::Utf8Bytes;
use serde::{Deserialize, Deserializer, Serialize};
use subtle::ConstantTimeEq;
use tokio::{sync::broadcast, time::Instant};

use crate::source::Source;

const MAX_ROOMS: usize = 256;
const MAX_PARTICIPANTS: usize = 24;
const UPDATE_BUFFER: usize = 16;
const ROOM_TTL_MS: u64 = 60 * 60 * 1000;

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum ClientMessage {
    Join {
        #[serde(default, deserialize_with = "present_token")]
        host_token: Option<String>,
    },
    Load { url: String },
    Play,
    Pause,
    Seek { position_sec: f64 },
    Ping { client_now_ms: f64 },
}

fn present_token<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    String::deserialize(d).map(Some)
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct WireError {
    code: &'static str,
    message: &'static str,
}

impl WireError {
    pub const INVALID_SOURCE: Self = Self::new("invalid_source", "Укажите ссылку на YouTube, MP4 или HLS.");
    pub const INVALID_MESSAGE: Self = Self::new("invalid_message", "Некорректное сообщение.");
    const ROOM_NOT_FOUND: Self = Self::new("room_not_found", "Комната не найдена или уже закрыта.");
    const INVALID_TOKEN: Self = Self::new("invalid_host_token", "Неверный ключ ведущего.");
    const FORBIDDEN: Self = Self::new("forbidden", "Управлять видео может только ведущий.");

    pub const fn new(code: &'static str, message: &'static str) -> Self {
        Self { code, message }
    }
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum ServerMessage<'a> {
    Joined { role: Role, state: &'a Playback, server_now_ms: u64 },
    State { state: &'a Playback, server_now_ms: u64 },
    Pong { client_now_ms: f64, server_now_ms: u64 },
    Error { #[serde(flatten)] error: WireError },
}

pub fn encode(message: ServerMessage<'_>) -> Utf8Bytes {
    serde_json::to_string(&message).expect("room messages are serializable").into()
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Host,
    Viewer,
}

enum PlaybackChange {
    Load(Source),
    SetPaused(bool),
    Seek(f64),
}

impl TryFrom<ClientMessage> for PlaybackChange {
    type Error = WireError;

    fn try_from(message: ClientMessage) -> Result<Self, Self::Error> {
        match message {
            ClientMessage::Load { url } => Source::parse(&url)
                .map(Self::Load)
                .ok_or(WireError::INVALID_SOURCE),
            ClientMessage::Play => Ok(Self::SetPaused(false)),
            ClientMessage::Pause => Ok(Self::SetPaused(true)),
            ClientMessage::Seek { position_sec } if position_sec.is_finite() && position_sec >= 0.0 => {
                Ok(Self::Seek(position_sec))
            }
            _ => Err(WireError::INVALID_MESSAGE),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Playback {
    source: Source,
    paused: bool,
    position_sec: f64,
    updated_at_ms: u64,
    revision: u64,
}

impl Playback {
    fn position_at(&self, now_ms: u64) -> f64 {
        if self.paused {
            self.position_sec
        } else {
            self.position_sec + (now_ms - self.updated_at_ms) as f64 / 1000.0
        }
    }

    fn apply(&mut self, change: PlaybackChange, now_ms: u64) {
        match change {
            PlaybackChange::Load(source) => {
                self.source = source;
                self.position_sec = 0.0;
                self.paused = true;
            }
            PlaybackChange::SetPaused(paused) => {
                self.position_sec = self.position_at(now_ms);
                self.paused = paused;
            }
            PlaybackChange::Seek(position_sec) => self.position_sec = position_sec,
        }
        self.updated_at_ms = now_ms;
        self.revision += 1;
    }
}

struct Room {
    host_token: String,
    playback: Playback,
    updates: broadcast::Sender<Utf8Bytes>,
    participants: usize,
    last_empty_ms: u64,
}

impl Room {
    fn is_available(&self, now_ms: u64) -> bool {
        self.participants > 0 || now_ms - self.last_empty_ms < ROOM_TTL_MS
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreatedRoom {
    room_id: String,
    host_token: String,
}

pub enum CreateFailure {
    Full,
    Entropy,
}

pub struct Rooms {
    entries: Mutex<HashMap<String, Room>>,
    started: Instant,
    epoch_ms: u64,
}

impl Rooms {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            started: Instant::now(),
            epoch_ms: SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64,
        }
    }

    pub fn now_ms(&self) -> u64 {
        // A wall-clock correction must not move the playback clock backward.
        self.epoch_ms + self.started.elapsed().as_millis() as u64
    }

    pub fn prune(&self) {
        let mut entries = self.entries.lock().unwrap();
        let now_ms = self.now_ms();
        entries.retain(|_, room| room.is_available(now_ms));
    }

    pub fn create(&self, source: Source) -> Result<CreatedRoom, CreateFailure> {
        loop {
            let mut bytes = [0; 32];
            getrandom::fill(&mut bytes).map_err(|_| CreateFailure::Entropy)?;
            let (id, token) = bytes.split_at(16);
            if id == token {
                continue;
            }
            let room_id = format!("{:032x}", u128::from_be_bytes(id.try_into().unwrap()));
            let host_token = format!("{:032x}", u128::from_be_bytes(token.try_into().unwrap()));
            let mut entries = self.entries.lock().unwrap();
            let now_ms = self.now_ms();
            if entries.len() >= MAX_ROOMS {
                entries.retain(|_, room| room.is_available(now_ms));
                if entries.len() >= MAX_ROOMS {
                    return Err(CreateFailure::Full);
                }
            }
            let Entry::Vacant(entry) = entries.entry(room_id.clone()) else { continue };
            entry.insert(Room {
                host_token: host_token.clone(),
                playback: Playback { source, paused: true, position_sec: 0.0, updated_at_ms: now_ms, revision: 0 },
                updates: broadcast::channel(UPDATE_BUFFER).0,
                participants: 0,
                last_empty_ms: now_ms,
            });
            return Ok(CreatedRoom { room_id, host_token });
        }
    }

    pub fn join(self: &Arc<Self>, room_id: String, host_token: Option<&str>) -> Result<Admission, WireError> {
        let mut entries = self.entries.lock().unwrap();
        let now_ms = self.now_ms();
        let room = entries.get_mut(&room_id).filter(|room| room.is_available(now_ms))
            .ok_or(WireError::ROOM_NOT_FOUND)?;
        let role = match host_token {
            None => Role::Viewer,
            Some(token) if bool::from(room.host_token.as_bytes().ct_eq(token.as_bytes())) => Role::Host,
            Some(_) => return Err(WireError::INVALID_TOKEN),
        };
        if room.participants >= MAX_PARTICIPANTS {
            return Err(WireError::new("invalid_message", "В комнате нет свободных мест."));
        }
        // Subscribe and take the snapshot under the same lock as command publication.
        let updates = room.updates.subscribe();
        let snapshot = encode(ServerMessage::Joined { role, state: &room.playback, server_now_ms: now_ms });
        room.participants += 1;
        drop(entries);
        Ok(Admission { participant: Participant { rooms: Arc::clone(self), room_id, role }, updates, snapshot })
    }
}

pub struct Admission {
    pub participant: Participant,
    pub updates: broadcast::Receiver<Utf8Bytes>,
    pub snapshot: Utf8Bytes,
}

pub struct Participant {
    rooms: Arc<Rooms>,
    room_id: String,
    role: Role,
}

impl Participant {
    pub fn command(&self, msg: ClientMessage) -> Result<(), WireError> {
        if self.role != Role::Host {
            return Err(WireError::FORBIDDEN);
        }
        let change = PlaybackChange::try_from(msg)?;
        let mut entries = self.rooms.entries.lock().unwrap();
        let now_ms = self.rooms.now_ms();
        let room = entries.get_mut(&self.room_id).expect("joined room cannot expire");
        let state = &mut room.playback;
        state.apply(change, now_ms);
        let _ = room.updates.send(encode(ServerMessage::State { state, server_now_ms: now_ms }));
        Ok(())
    }

    pub fn resync(&self) -> (broadcast::Receiver<Utf8Bytes>, Utf8Bytes) {
        let entries = self.rooms.entries.lock().unwrap();
        let room = &entries[&self.room_id];
        let state = encode(ServerMessage::State { state: &room.playback, server_now_ms: self.rooms.now_ms() });
        (room.updates.subscribe(), state)
    }
}

impl Drop for Participant {
    fn drop(&mut self) {
        let mut entries = self.rooms.entries.lock().unwrap();
        let room = entries.get_mut(&self.room_id).expect("joined room cannot expire");
        room.participants -= 1;
        if room.participants == 0 {
            room.last_empty_ms = self.rooms.now_ms();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use broadcast::error::TryRecvError;
    use serde_json::Value;

    fn room() -> (Arc<Rooms>, CreatedRoom) {
        let rooms = Arc::new(Rooms::new());
        let source = Source::parse("https://media.example/video.mp4").unwrap();
        let created = rooms.create(source).ok().expect("create room");
        (rooms, created)
    }

    fn json(event: Utf8Bytes) -> Value {
        serde_json::from_str(event.as_str()).unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn only_host_can_mutate_and_late_join_projects_position() {
        let (rooms, created) = room();
        let id = &created.room_id;
        assert!(matches!(rooms.join(id.clone(), Some(id)), Err(e) if e.code == "invalid_host_token"));
        assert!(matches!(rooms.join("missing".into(), None), Err(e) if e.code == "room_not_found"));
        let mut host = rooms.join(id.clone(), Some(&created.host_token)).unwrap();
        assert_eq!(json(host.snapshot)["role"], "host");
        let mut viewer = rooms.join(id.clone(), None).unwrap();
        assert_eq!(json(viewer.snapshot)["role"], "viewer");
        assert_eq!(viewer.participant.command(ClientMessage::Play).unwrap_err().code, "forbidden");
        assert!(matches!(host.updates.try_recv(), Err(TryRecvError::Empty)));

        host.participant.command(ClientMessage::Seek { position_sec: 12.5 }).unwrap();
        host.participant.command(ClientMessage::Play).unwrap();
        assert_eq!(json(host.updates.try_recv().unwrap()), json(viewer.updates.try_recv().unwrap()));
        let playing = json(host.updates.try_recv().unwrap());
        assert_eq!(playing, json(viewer.updates.try_recv().unwrap()));
        assert_eq!(playing["type"], "state");
        assert_eq!(playing["state"]["revision"], 2);

        tokio::time::advance(Duration::from_secs(5)).await;
        let late = rooms.join(id.clone(), None).unwrap();
        let snapshot = json(late.snapshot);
        assert_eq!(snapshot["type"], "joined");
        assert_eq!(snapshot["state"], playing["state"]);
        let elapsed_ms = snapshot["serverNowMs"].as_u64().unwrap() - snapshot["state"]["updatedAtMs"].as_u64().unwrap();
        assert_eq!(elapsed_ms, 5000);
        host.participant.command(ClientMessage::Pause).unwrap();
        let paused = json(host.updates.try_recv().unwrap());
        assert_eq!(paused["state"]["positionSec"], 17.5);
        assert_eq!(paused["state"]["paused"], true);
        assert_eq!(paused["state"]["revision"], 3);
    }

    #[tokio::test(start_paused = true)]
    async fn lagged_viewer_skips_stale_revisions_and_joined_room_cannot_expire() {
        let (rooms, created) = room();
        let id = &created.room_id;
        let host = rooms.join(id.clone(), Some(&created.host_token)).unwrap();
        let mut viewer = rooms.join(id.clone(), None).unwrap();
        for i in 0..=UPDATE_BUFFER {
            host.participant.command(ClientMessage::Seek { position_sec: i as f64 }).unwrap();
        }
        assert!(matches!(viewer.updates.try_recv(), Err(TryRecvError::Lagged(_))));
        let (mut fresh, snapshot) = viewer.participant.resync();
        assert_eq!(json(snapshot)["state"]["revision"], UPDATE_BUFFER + 1);
        assert!(matches!(fresh.try_recv(), Err(TryRecvError::Empty)));

        tokio::time::advance(Duration::from_millis(ROOM_TTL_MS)).await;
        rooms.prune();
        drop(host);
        drop(viewer);
        assert!(rooms.join(id.clone(), None).is_ok());
        tokio::time::advance(Duration::from_millis(ROOM_TTL_MS)).await;
        assert!(matches!(rooms.join(id.clone(), None), Err(e) if e.code == "room_not_found"));
        rooms.prune();
        assert!(rooms.entries.lock().unwrap().is_empty());
    }
}
