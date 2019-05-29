use chashmap::CHashMap;
use crossbeam::channel::unbounded;
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use std::{sync::atomic::AtomicUsize, sync::atomic::Ordering, sync::Arc};
use ws::{CloseCode, Handler, Handshake, Result};

const PBKDF2_ITERATIONS: u32 = 1;
const RANGE_LATLON: f32 = 0.1;
const RANGE_KM: f32 = 10.0;

#[derive(Serialize, Deserialize)]
pub enum JsonMessage {
    Location { lat: f32, lon: f32 },
    Login { username: String, password: String },
    LoginResponse { status: bool },
    Register { username: String, password: String },
    RegisterResponse { status: bool },
    SendMessage { msg: String },
    Message { username: String, msg: String },
    Error { reason: String },
}

pub enum Message {
    Open {
        server: Server,
        tx: crossbeam::Sender<usize>,
    },
    Close {
        id: usize,
        code: ws::CloseCode,
    },
    Login {
        id: usize,
        username: String,
        password: String,
        tx: crossbeam::Sender<JsonMessage>,
    },
    Register {
        id: usize,
        username: String,
        password: String,
        tx: crossbeam::Sender<JsonMessage>,
    },
    Message {
        user_id: usize,
        msg: String,
    },
    Location {
        user_id: usize,
        lat: f32,
        lon: f32,
    },
}

pub struct User {
    pub id: usize,
    pub name: String,
    pub lat: f32,
    pub lon: f32,
    pub password: String,
}

impl User {
    fn new(id: usize, name: String, password: String) -> User {
        User {
            id,
            name,
            lat: 0.0,
            lon: 0.0,
            password,
        }
    }

    fn distance_to(&self, other: &User) -> f32 {
        let mut ph1: f32 = self.lat - other.lat;
        let mut th1: f32 = self.lon;
        let mut th2: f32 = other.lon;

        ph1 = ph1.to_radians();
        th1 = th1.to_radians();
        th2 = th2.to_radians();
        let dz: f32 = th1.sin() - th2.sin();
        let dx: f32 = ph1.cos() * th1.cos() - th2.cos();
        let dy: f32 = ph1.sin() * th1.cos();
        ((dx * dx + dy * dy + dz * dz).sqrt() / 2.0).asin() * 2.0 * 6372.8
    }

    fn within_bounds(&self, other: &User, diff: f32) -> bool {
        (self.lat - other.lat).abs() < diff && (self.lon - other.lon).abs() < diff
    }
}

#[derive(Clone)]
pub struct Users {
    current_id: Arc<AtomicUsize>,
    users: Arc<CHashMap<usize, User>>,
    users_by_name: Arc<CHashMap<String, usize>>,
}

impl Users {
    pub fn new() -> Self {
        Users {
            current_id: Arc::new(AtomicUsize::new(0)),
            users: Arc::new(CHashMap::new()),
            users_by_name: Arc::new(CHashMap::new()),
        }
    }

    pub fn contains_username(&self, username: &str) -> bool {
        self.users_by_name.contains_key(username)
    }

    pub fn add(&self, username: &str, password: &str) -> usize {
        let c_id = self.current_id.fetch_add(1, Ordering::Relaxed);

        let user = User::new(
            c_id,
            username.to_string(),
            pbkdf2::pbkdf2_simple(&password, PBKDF2_ITERATIONS).unwrap(),
        );

        self.users.insert(c_id, user);
        self.users_by_name.insert(username.to_string(), c_id);

        c_id
    }

    pub fn get_by_id(&self, id: usize) -> Option<chashmap::ReadGuard<'_, usize, User>> {
        self.users.get(&id)
    }

    pub fn get_mut_by_id(&self, id: usize) -> Option<chashmap::WriteGuard<'_, usize, User>> {
        self.users.get_mut(&id)
    }

    pub fn get_by_name(&self, username: &str) -> Option<chashmap::ReadGuard<'_, usize, User>> {
        let user_id = self.users_by_name.get(username);
        match user_id {
            Some(user_id) => self.users.get(&user_id),
            None => None,
        }
    }

    pub fn in_range(&self, id_1: usize, id_2: usize) -> bool {
        if let Some(user_1) = self.users.get(&id_1) {
            if let Some(user_2) = self.users.get(&id_2) {
                return user_1.within_bounds(&user_2, RANGE_LATLON)
                    && user_1.distance_to(&user_2) < RANGE_KM;
            }
        }

        false
    }
}

#[derive(Clone)]
pub struct Servers {
    current_id: Arc<AtomicUsize>,
    reader: evmap::ReadHandle<usize, Server>,
    writer: Arc<Mutex<evmap::WriteHandle<usize, Server>>>,
}

impl Servers {
    pub fn new() -> Self {
        let (reader, writer) = evmap::new();
        Servers {
            current_id: Arc::new(AtomicUsize::new(0)),
            reader,
            writer: Arc::new(Mutex::new(writer)),
        }
    }

    /*
    fn write(&self) -> MutexGuard<'_, evmap::WriteHandle<usize, Server>, > {
        self.writer.lock()
    }
    */

    pub fn read(&self) -> &evmap::ReadHandle<usize, Server> {
        &self.reader
    }

    pub fn update(&self, id: usize, server: Server) {
        self.writer.lock().update(id, server).refresh();
    }

    pub fn empty(&self, id: usize) {
        self.writer.lock().empty(id).refresh();
    }

    pub fn get_next_id(&self) -> usize {
        self.current_id.fetch_add(1, Ordering::Relaxed)
    }

    pub fn get(&self, id: usize) -> Option<Server> {
        self.reader
            .get_and(&id, |rs| match rs.first() {
                Some(rs) => Some(rs.clone()),
                _ => None,
            })
            .unwrap_or_else(|| None)
    }

    pub fn len(&self) -> usize {
        self.reader.len()
    }
}

// Server web application handler
#[derive(Clone)]
pub struct Server {
    pub id: usize,
    pub user_id: Arc<RwLock<Option<usize>>>,
    pub socket: ws::Sender,
    pub channel: crossbeam::Sender<Message>,
}

impl Eq for Server {}

impl PartialEq for Server {
    fn eq(&self, other: &Server) -> bool {
        self.socket.token() == other.socket.token()
    }
}

impl evmap::ShallowCopy for Server {
    unsafe fn shallow_copy(&mut self) -> Self {
        Server {
            id: self.id,
            user_id: self.user_id.clone(),
            socket: self.socket.clone(),
            channel: self.channel.clone(),
        }
    }
}

impl Handler for Server {
    fn on_open(&mut self, _shake: Handshake) -> Result<()> {
        let (tx, rx) = unbounded();
        let _ = self.channel.send(Message::Open {
            server: self.clone(),
            tx,
        });

        if let Ok(id) = rx.recv() {
            self.id = id;
        }

        Ok(())
    }

    fn on_message(&mut self, msg: ws::Message) -> Result<()> {
        let (tx, rx) = unbounded();

        if let Ok(s) = msg.as_text() {
            if let Ok(val) = serde_json::from_str(s) {
                let val: JsonMessage = val;
                match val {
                    JsonMessage::Location { lat, lon } => {
                        if let Some(user_id) = *self.user_id.read() {
                            let _ = self.channel.send(Message::Location { user_id, lat, lon });
                        }
                    }
                    JsonMessage::Login { username, password } => {
                        let _ = self.channel.send(Message::Login {
                            id: self.id,
                            username,
                            password,
                            tx,
                        });

                        if let Ok(response) = rx.recv() {
                            if let Ok(json) = serde_json::to_string(&response) {
                                let _ = self.socket.send(json);
                            }
                        }
                    }
                    JsonMessage::Register { username, password } => {
                        let _ = self.channel.send(Message::Register {
                            id: self.id,
                            username,
                            password,
                            tx,
                        });

                        if let Ok(response) = rx.recv() {
                            if let Ok(json) = serde_json::to_string(&response) {
                                let _ = self.socket.send(json);
                            }
                        }
                    }
                    JsonMessage::SendMessage { msg } => {
                        if msg.len() <= 300 {
                            if let Some(user_id) = *self.user_id.read() {
                                let _ = self.channel.send(Message::Message { user_id, msg });
                            }
                        }
                    }
                    _ => (),
                }
            }
        }

        Ok(())
    }

    fn on_close(&mut self, code: CloseCode, _reason: &str) {
        let _ = self.channel.send(Message::Close { id: self.id, code });
        let _ = self.socket.close(CloseCode::Normal);
    }
}
