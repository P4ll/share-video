use std::{
    collections::{HashMap, hash_map::Entry},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use axum::extract::ws::Utf8Bytes;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Deserializer, Serialize};
use subtle::ConstantTimeEq;
use tokio::{sync::broadcast, time::Instant};

use crate::source::Source;

const MAX_ROOMS: usize = 256;
const MAX_PARTICIPANTS: usize = 24;
const MAX_INVITATIONS: usize = 23;
const UPDATE_BUFFER: usize = 16;
const ROOM_TTL_MS: u64 = 60 * 60 * 1000;

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum ClientMessage {
    Join {
        #[serde(default, deserialize_with = "present_token")]
        host_token: Option<String>,
        #[serde(default, deserialize_with = "present_token")]
        guest_token: Option<String>,
    },
    Load { url: String },
    Play,
    Pause,
    Seek { position_sec: f64 },
    CreateInvitation,
    SetGuestControl { guest_number: usize, can_control: bool },
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
    const INVALID_HOST_TOKEN: Self = Self::new("invalid_host_token", "Неверный ключ ведущего.");
    const INVALID_GUEST_TOKEN: Self = Self::new("invalid_guest_token", "Неверное приглашение.");
    const FORBIDDEN: Self = Self::new("forbidden", "У вас нет разрешения на это действие.");
    const INVITATION_CAPACITY: Self = Self::new("invitation_capacity", "Достигнут предел приглашений.");
    const INVITATION_NOT_FOUND: Self = Self::new("invitation_not_found", "Приглашение не найдено.");

    pub const fn new(code: &'static str, message: &'static str) -> Self {
        Self { code, message }
    }
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum ServerMessage<'a> {
    Joined {
        role: Role,
        state: &'a Playback,
        server_now_ms: u64,
        #[serde(flatten)]
        access: Access<'a>,
    },
    Access { #[serde(flatten)] access: Access<'a> },
    State { state: &'a Playback, server_now_ms: u64 },
    Pong { client_now_ms: f64, server_now_ms: u64 },
    Error { #[serde(flatten)] error: WireError },
}

pub fn encode(message: ServerMessage<'_>) -> Utf8Bytes {
    serde_json::to_string(&message).expect("room messages are serializable").into()
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Host,
    Guest,
}

#[derive(Serialize)]
#[serde(untagged, rename_all_fields = "camelCase")]
pub enum Access<'a> {
    Host { can_control: bool, invitations: &'a [Invitation] },
    Guest { can_control: bool, guest_number: usize },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Invitation {
    number: usize,
    token: String,
    can_control: bool,
}

#[derive(Clone, Copy)]
enum Identity {
    Host,
    Guest(usize),
}

impl Identity {
    fn access(self, room: &Room) -> Access<'_> {
        match self {
            Self::Host => Access::Host { can_control: true, invitations: &room.invitations },
            Self::Guest(index) => {
                let invitation = &room.invitations[index];
                Access::Guest { can_control: invitation.can_control, guest_number: invitation.number }
            }
        }
    }
}

#[derive(Clone)]
pub enum Update {
    Playback(Utf8Bytes),
    AccessChanged,
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
    invitations: Vec<Invitation>,
    updates: broadcast::Sender<Update>,
    participants: usize,
    last_empty_ms: u64,
}

impl Room {
    fn is_available(&self, now_ms: u64) -> bool {
        self.participants > 0 || now_ms - self.last_empty_ms < ROOM_TTL_MS
    }

    fn create_invitation(&mut self, room_id: &str, invite_rooms: &mut HashMap<String, String>) -> Result<(), WireError> {
        if self.invitations.len() >= MAX_INVITATIONS {
            return Err(WireError::INVITATION_CAPACITY);
        }
        loop {
            let mut bytes = [0; 16];
            getrandom::fill(&mut bytes)
                .map_err(|_| WireError::new("internal_error", "Не удалось создать приглашение."))?;
            let token = URL_SAFE_NO_PAD.encode(bytes);
            if let Entry::Vacant(entry) = invite_rooms.entry(token.clone()) {
                entry.insert(room_id.to_owned());
                self.invitations.push(Invitation { number: self.invitations.len() + 1, token, can_control: false });
                break;
            }
        }
        Ok(())
    }

    fn invitation_index(&self, token: &str) -> Option<usize> {
        self.invitations.iter().position(|item| bool::from(item.token.as_bytes().ct_eq(token.as_bytes())))
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

#[derive(Default)]
struct Registry {
    rooms: HashMap<String, Room>,
    invite_rooms: HashMap<String, String>,
}

impl Registry {
    fn prune(&mut self, now_ms: u64) {
        self.rooms.retain(|_, room| {
            if room.is_available(now_ms) {
                return true;
            }
            for invitation in &room.invitations {
                self.invite_rooms.remove(&invitation.token);
            }
            false
        });
    }
}

pub struct Rooms {
    entries: Mutex<Registry>,
    started: Instant,
    epoch_ms: u64,
}

impl Rooms {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(Registry::default()),
            started: Instant::now(),
            epoch_ms: SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64,
        }
    }

    pub fn now_ms(&self) -> u64 {
        // A wall-clock correction must not move the playback clock backward.
        self.epoch_ms + self.started.elapsed().as_millis() as u64
    }

    pub fn prune(&self) {
        self.entries.lock().unwrap().prune(self.now_ms());
    }

    pub fn invite_room(&self, token: &str) -> Option<String> {
        // A 128-bit token is exactly 22 characters without padding.
        if token.len() != 22 {
            return None;
        }
        let entries = self.entries.lock().unwrap();
        let room_id = entries.invite_rooms.get(token)?;
        entries.rooms.get(room_id)?.is_available(self.now_ms()).then(|| room_id.clone())
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
            if entries.rooms.len() >= MAX_ROOMS {
                entries.prune(now_ms);
                if entries.rooms.len() >= MAX_ROOMS {
                    return Err(CreateFailure::Full);
                }
            }
            let Entry::Vacant(entry) = entries.rooms.entry(room_id.clone()) else { continue };
            entry.insert(Room {
                host_token: host_token.clone(),
                playback: Playback { source, paused: true, position_sec: 0.0, updated_at_ms: now_ms, revision: 0 },
                invitations: Vec::new(),
                updates: broadcast::channel(UPDATE_BUFFER).0,
                participants: 0,
                last_empty_ms: now_ms,
            });
            return Ok(CreatedRoom { room_id, host_token });
        }
    }

    pub fn join(
        self: &Arc<Self>,
        room_id: String,
        host_token: Option<&str>,
        guest_token: Option<&str>,
    ) -> Result<Admission, WireError> {
        if host_token.is_some() && guest_token.is_some() {
            return Err(WireError::INVALID_MESSAGE);
        }
        let mut entries = self.entries.lock().unwrap();
        let now_ms = self.now_ms();
        let room = entries.rooms.get_mut(&room_id).filter(|room| room.is_available(now_ms))
            .ok_or(WireError::ROOM_NOT_FOUND)?;
        let identity = match host_token {
            Some(token) if bool::from(room.host_token.as_bytes().ct_eq(token.as_bytes())) => Identity::Host,
            Some(_) => return Err(WireError::INVALID_HOST_TOKEN),
            None => Identity::Guest(
                guest_token.and_then(|token| room.invitation_index(token)).ok_or(WireError::INVALID_GUEST_TOKEN)?,
            ),
        };
        if room.participants >= MAX_PARTICIPANTS {
            return Err(WireError::new("invalid_message", "В комнате нет свободных мест."));
        }
        // Subscribe and take the snapshot under the same lock as command publication.
        let updates = room.updates.subscribe();
        let snapshot = encode(ServerMessage::Joined {
            role: match identity {
                Identity::Host => Role::Host,
                Identity::Guest(_) => Role::Guest,
            },
            state: &room.playback,
            server_now_ms: now_ms,
            access: identity.access(room),
        });
        room.participants += 1;
        drop(entries);
        Ok(Admission { participant: Participant { rooms: Arc::clone(self), room_id, identity }, updates, snapshot })
    }
}

pub struct Admission {
    pub participant: Participant,
    pub updates: broadcast::Receiver<Update>,
    pub snapshot: Utf8Bytes,
}

pub struct Participant {
    rooms: Arc<Rooms>,
    room_id: String,
    identity: Identity,
}

impl Participant {
    pub fn command(&self, msg: ClientMessage) -> Result<(), WireError> {
        let mut entries = self.rooms.entries.lock().unwrap();
        let Registry { rooms, invite_rooms } = &mut *entries;
        let room = rooms.get_mut(&self.room_id).expect("joined room cannot expire");
        let update = match msg {
            ClientMessage::Load { .. } | ClientMessage::CreateInvitation | ClientMessage::SetGuestControl { .. }
                if !matches!(self.identity, Identity::Host) => return Err(WireError::FORBIDDEN),
            ClientMessage::CreateInvitation => {
                room.create_invitation(&self.room_id, invite_rooms)?;
                Update::AccessChanged
            }
            ClientMessage::SetGuestControl { guest_number, can_control } => {
                let invitation = guest_number.checked_sub(1).and_then(|i| room.invitations.get_mut(i))
                    .ok_or(WireError::INVITATION_NOT_FOUND)?;
                if invitation.can_control == can_control {
                    return Ok(());
                }
                invitation.can_control = can_control;
                Update::AccessChanged
            }
            msg => {
                if let Identity::Guest(index) = self.identity && !room.invitations[index].can_control {
                    return Err(WireError::FORBIDDEN);
                }
                let change = PlaybackChange::try_from(msg)?;
                let server_now_ms = self.rooms.now_ms();
                room.playback.apply(change, server_now_ms);
                Update::Playback(encode(ServerMessage::State { state: &room.playback, server_now_ms }))
            }
        };
        let _ = room.updates.send(update);
        Ok(())
    }

    pub fn access(&self) -> Utf8Bytes {
        let entries = self.rooms.entries.lock().unwrap();
        self.encode_access(&entries.rooms[&self.room_id])
    }

    fn encode_access(&self, room: &Room) -> Utf8Bytes {
        encode(ServerMessage::Access { access: self.identity.access(room) })
    }

    pub fn resync(&self) -> (broadcast::Receiver<Update>, Utf8Bytes, Utf8Bytes) {
        let entries = self.rooms.entries.lock().unwrap();
        let room = &entries.rooms[&self.room_id];
        let state = encode(ServerMessage::State { state: &room.playback, server_now_ms: self.rooms.now_ms() });
        (room.updates.subscribe(), self.encode_access(room), state)
    }
}

impl Drop for Participant {
    fn drop(&mut self) {
        let mut entries = self.rooms.entries.lock().unwrap();
        let room = entries.rooms.get_mut(&self.room_id).expect("joined room cannot expire");
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
    use serde_json::{Value, json};

    fn room() -> (Arc<Rooms>, CreatedRoom) {
        let rooms = Arc::new(Rooms::new());
        let source = Source::parse("https://media.example/video.mp4").unwrap();
        let created = rooms.create(source).ok().expect("create room");
        (rooms, created)
    }

    fn json(event: &Utf8Bytes) -> Value {
        serde_json::from_str(event.as_str()).unwrap()
    }

    fn access(admission: &mut Admission) -> Value {
        assert!(matches!(admission.updates.try_recv(), Ok(Update::AccessChanged)));
        json(&admission.participant.access())
    }

    fn invite(host: &mut Admission) -> Value {
        host.participant.command(ClientMessage::CreateInvitation).unwrap();
        access(host)["invitations"].as_array().unwrap().last().unwrap().clone()
    }

    fn playback(admission: &mut Admission) -> Value {
        let Update::Playback(text) = admission.updates.try_recv().unwrap() else {
            panic!("expected playback update");
        };
        json(&text)
    }

    #[tokio::test(start_paused = true)]
    async fn late_guest_join_projects_position() {
        let (rooms, created) = room();
        let id = &created.room_id;
        assert!(matches!(rooms.join(id.clone(), Some(id), None), Err(e) if e.code == "invalid_host_token"));
        assert!(matches!(rooms.join(id.clone(), None, Some(id)), Err(e) if e.code == "invalid_guest_token"));
        assert!(matches!(rooms.join(id.clone(), None, None), Err(e) if e.code == "invalid_guest_token"));
        assert!(matches!(rooms.join(id.clone(), Some(id), Some(id)), Err(e) if e.code == "invalid_message"));
        assert!(matches!(rooms.join("missing".into(), None, None), Err(e) if e.code == "room_not_found"));
        let mut host = rooms.join(id.clone(), Some(&created.host_token), None).unwrap();
        let invitation = invite(&mut host);
        let token = invitation["token"].as_str().unwrap();
        let mut guest = rooms.join(id.clone(), None, Some(token)).unwrap();
        assert_eq!(guest.participant.command(ClientMessage::Play).unwrap_err().code, "forbidden");
        assert!(matches!(host.updates.try_recv(), Err(TryRecvError::Empty)));

        host.participant.command(ClientMessage::Seek { position_sec: 12.5 }).unwrap();
        host.participant.command(ClientMessage::Play).unwrap();
        assert_eq!(playback(&mut host), playback(&mut guest));
        let playing = playback(&mut host);
        assert_eq!(playing, playback(&mut guest));
        assert_eq!(playing["type"], "state");
        assert_eq!(playing["state"]["revision"], 2);

        tokio::time::advance(Duration::from_secs(5)).await;
        let late = rooms.join(id.clone(), None, Some(token)).unwrap();
        let snapshot = json(&late.snapshot);
        assert_eq!(snapshot["type"], "joined");
        assert_eq!(snapshot["state"], playing["state"]);
        let elapsed_ms = snapshot["serverNowMs"].as_u64().unwrap() - snapshot["state"]["updatedAtMs"].as_u64().unwrap();
        assert_eq!(elapsed_ms, 5000);
        host.participant.command(ClientMessage::Pause).unwrap();
        let paused = playback(&mut host);
        assert_eq!(paused["state"]["positionSec"], 17.5);
        assert_eq!(paused["state"]["paused"], true);
        assert_eq!(paused["state"]["revision"], 3);
    }

    #[tokio::test(start_paused = true)]
    async fn connected_guests_have_independent_permissions_and_private_access() {
        let (rooms, created) = room();
        let id = &created.room_id;
        let mut host = rooms.join(id.clone(), Some(&created.host_token), None).unwrap();
        let host_joined = json(&host.snapshot);
        assert_eq!(host_joined["role"], "host");
        assert_eq!(host_joined["canControl"], true);
        assert_eq!(host_joined["invitations"], json!([]));
        assert!(host_joined.get("guestNumber").is_none());

        let first_invite = invite(&mut host);
        let second_invite = invite(&mut host);
        let first_token = first_invite["token"].as_str().unwrap();
        let second_token = second_invite["token"].as_str().unwrap();
        assert_ne!(first_token, second_token);
        assert_eq!(first_invite, json!({"number": 1, "token": first_token, "canControl": false}));
        assert_eq!(second_invite, json!({"number": 2, "token": second_token, "canControl": false}));
        let mut first = rooms.join(id.clone(), None, Some(first_token)).unwrap();
        let mut second = rooms.join(id.clone(), None, Some(second_token)).unwrap();
        for (guest, number) in [(&first, 1), (&second, 2)] {
            assert_eq!(json(&guest.snapshot), json!({
                "type": "joined", "role": "guest", "guestNumber": number, "canControl": false,
                "state": host_joined["state"], "serverNowMs": host_joined["serverNowMs"]
            }));
        }

        let participant = &host.participant;
        let error = participant.command(ClientMessage::SetGuestControl { guest_number: 3, can_control: true }).unwrap_err();
        assert_eq!(error.code, "invitation_not_found");
        participant.command(ClientMessage::SetGuestControl { guest_number: 1, can_control: true }).unwrap();
        // Authorization changes before either guest has received their update.
        first.participant.command(ClientMessage::Seek { position_sec: 8.0 }).unwrap();
        assert_eq!(second.participant.command(ClientMessage::Play).unwrap_err().code, "forbidden");
        assert_eq!(access(&mut first), json!({"type": "access", "guestNumber": 1, "canControl": true}));
        assert_eq!(access(&mut second), json!({"type": "access", "guestNumber": 2, "canControl": false}));
        assert_eq!(access(&mut host), json!({
            "type": "access", "canControl": true, "invitations": [
                {"number": 1, "token": first_token, "canControl": true}, second_invite
            ]
        }));
        assert_eq!(playback(&mut second)["state"]["positionSec"], 8.0);

        first.participant.command(ClientMessage::Play).unwrap();
        assert_eq!(playback(&mut second)["state"]["paused"], false);
        first.participant.command(ClientMessage::Pause).unwrap();
        assert_eq!(playback(&mut second)["state"]["paused"], true);
        for command in [
            ClientMessage::Load { url: "https://media.example/other.mp4".into() },
            ClientMessage::CreateInvitation,
            ClientMessage::SetGuestControl { guest_number: 2, can_control: true },
        ] {
            assert_eq!(first.participant.command(command).unwrap_err().code, "forbidden");
        }

        host.participant.command(ClientMessage::SetGuestControl { guest_number: 1, can_control: false }).unwrap();
        assert_eq!(first.participant.command(ClientMessage::Play).unwrap_err().code, "forbidden");
        for _ in 0..3 {
            playback(&mut first);
        }
        assert_eq!(access(&mut first), json!({"type": "access", "guestNumber": 1, "canControl": false}));
        assert_eq!(access(&mut second), json!({"type": "access", "guestNumber": 2, "canControl": false}));
        assert!(matches!(second.updates.try_recv(), Err(TryRecvError::Empty)));
        let rejoined = rooms.join(id.clone(), None, Some(first_token)).unwrap();
        assert_eq!(json(&rejoined.snapshot)["guestNumber"], 1);
        assert_eq!(json(&rejoined.snapshot)["canControl"], false);
    }

    #[test]
    fn invitation_capacity_is_separate_from_socket_capacity() {
        let (rooms, created) = room();
        let id = &created.room_id;
        let mut host = rooms.join(id.clone(), Some(&created.host_token), None).unwrap();
        let invitation = invite(&mut host);
        let token = invitation["token"].as_str().unwrap();
        let guests: Vec<_> = (1..24).map(|_| rooms.join(id.clone(), None, Some(token)).unwrap()).collect();
        for _ in 1..23 {
            host.participant.command(ClientMessage::CreateInvitation).unwrap();
        }
        assert_eq!(host.participant.command(ClientMessage::CreateInvitation).unwrap_err().code, "invitation_capacity");
        assert!(matches!(rooms.join(id.clone(), None, Some(token)), Err(e) if e.code == "invalid_message"));
        drop(guests);
        assert!(rooms.join(id.clone(), None, Some(token)).is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn lagged_guest_resyncs_private_access_and_playback_and_joined_room_cannot_expire() {
        let (rooms, created) = room();
        let id = &created.room_id;
        let mut host = rooms.join(id.clone(), Some(&created.host_token), None).unwrap();
        let invitation = invite(&mut host);
        invite(&mut host);
        let token = invitation["token"].as_str().unwrap();
        let mut guest = rooms.join(id.clone(), None, Some(token)).unwrap();
        host.participant.command(ClientMessage::SetGuestControl { guest_number: 1, can_control: true }).unwrap();
        for i in 0..=UPDATE_BUFFER {
            host.participant.command(ClientMessage::Seek { position_sec: i as f64 }).unwrap();
        }
        assert!(matches!(guest.updates.try_recv(), Err(TryRecvError::Lagged(_))));
        let (mut fresh, access, snapshot) = guest.participant.resync();
        assert_eq!(json(&access), json!({"type": "access", "guestNumber": 1, "canControl": true}));
        assert_eq!(json(&snapshot)["state"]["revision"], UPDATE_BUFFER + 1);
        assert!(matches!(fresh.try_recv(), Err(TryRecvError::Empty)));

        tokio::time::advance(Duration::from_millis(ROOM_TTL_MS)).await;
        rooms.prune();
        assert_eq!(rooms.invite_room(token).as_deref(), Some(id.as_str()));
        drop(host);
        drop(guest);
        assert!(rooms.join(id.clone(), None, Some(token)).is_ok());
        tokio::time::advance(Duration::from_millis(ROOM_TTL_MS)).await;
        assert!(matches!(rooms.join(id.clone(), None, Some(token)), Err(e) if e.code == "room_not_found"));
        assert!(rooms.invite_room(token).is_none());
        rooms.prune();
        let registry = rooms.entries.lock().unwrap();
        assert!(registry.rooms.is_empty());
        assert!(registry.invite_rooms.is_empty());
    }
}
