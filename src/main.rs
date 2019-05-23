extern crate chashmap;
extern crate crossbeam;
extern crate crossbeam_utils;
extern crate evmap;
extern crate parking_lot;
extern crate pbkdf2;
extern crate serde;
extern crate serde_json;
extern crate ws;

use chashmap::CHashMap;
use crossbeam::channel::unbounded;
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use std::{sync::atomic::AtomicUsize, sync::atomic::Ordering, sync::Arc, thread};
use ws::{CloseCode, Handler, Handshake, Result};

use std::any::Any;

const PBKDF2_ITERATIONS: u32 = 1;
const ENDPOINT: &str = "127.0.0.1:3012";
const WORKERS: usize = 4;

#[derive(Serialize, Deserialize)]
enum JsonMessage {
    Location { lat: f32, lon: f32 },
    Login { username: String, password: String },
    LoginResponse { status: bool },
    Register { username: String, password: String },
    RegisterResponse { status: bool },
    SendMessage { msg: String },
    Message { username: String, msg: String },
    Error { reason: String },
}

enum Message {
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

struct User {
    id: usize,
    name: String,
    lat: f32,
    lon: f32,
    password: String,
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
struct Users {
    current_id: Arc<AtomicUsize>,
    users: Arc<CHashMap<usize, User>>,
    users_by_name: Arc<CHashMap<String, usize>>,
}

impl Users {
    fn new() -> Self {
        Users {
            current_id: Arc::new(AtomicUsize::new(0)),
            users: Arc::new(CHashMap::new()),
            users_by_name: Arc::new(CHashMap::new()),
        }
    }

    fn contains_username(&self, username: &str) -> bool {
        self.users_by_name.contains_key(username)
    }

    fn add(&self, username: &str, password: &str) -> usize {
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

    fn get_by_id(&self, id: usize) -> Option<chashmap::ReadGuard<'_, usize, User>> {
        self.users.get(&id)
    }

    fn get_mut_by_id(&self, id: usize) -> Option<chashmap::WriteGuard<'_, usize, User>> {
        self.users.get_mut(&id)
    }

    fn get_by_name(&self, username: &str) -> Option<chashmap::ReadGuard<'_, usize, User>> {
        let user_id = self.users_by_name.get(username);
        match user_id {
            Some(user_id) => self.users.get(&user_id),
            None => None,
        }
    }

    fn in_range(&self, id_1: usize, id_2: usize) -> bool {
        if let Some(user_1) = self.users.get(&id_1) {
            if let Some(user_2) = self.users.get(&id_2) {
                return user_1.within_bounds(&user_2, 0.1) && user_1.distance_to(&user_2) < 10.0;
            }
        }

        false
    }
}

#[derive(Clone)]
struct Servers {
    current_id: Arc<AtomicUsize>,
    reader: evmap::ReadHandle<usize, Server>,
    writer: Arc<Mutex<evmap::WriteHandle<usize, Server>>>,
}

impl Servers {
    fn new() -> Self {
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

    fn read(&self) -> &evmap::ReadHandle<usize, Server> {
        &self.reader
    }

    fn update(&self, id: usize, server: Server) {
        self.writer.lock().update(id, server).refresh();
    }

    fn empty(&self, id: usize) {
        self.writer.lock().empty(id).refresh();
    }

    fn get_next_id(&self) -> usize {
        self.current_id.fetch_add(1, Ordering::Relaxed)
    }

    fn get(&self, id: usize) -> Option<Server> {
        self.reader
            .get_and(&id, |rs| match rs.first() {
                Some(rs) => Some(rs.clone()),
                _ => None,
            })
            .unwrap_or_else(|| None)
    }

    fn len(&self) -> usize {
        self.reader.len()
    }
}

// Server web application handler
#[derive(Clone)]
struct Server {
    id: usize,
    user_id: Arc<RwLock<Option<usize>>>,
    socket: ws::Sender,
    channel: crossbeam::Sender<Message>,
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

fn main() {
    let (tx, rx) = unbounded();

    let users = Users::new();
    let servers = Servers::new();

    let (t_tx, t_rx) = unbounded();

    let mut threads = Vec::new();

    threads.push(thread::spawn(move || {
        if let Ok(socket) = ws::Builder::new()
            .with_settings(ws::Settings {
                max_connections: 100_000,
                ..ws::Settings::default()
            })
            .build(|out| Server {
                id: 0,
                user_id: Arc::new(RwLock::new(None)),
                socket: out,
                channel: tx.clone(),
            })
        {
            let _ = socket.listen(ENDPOINT);
        }
    }));

    for i in 0..WORKERS {
        let t_rx = t_rx.clone();
        let users = users.clone();
        let servers = servers.clone();

        threads.push(thread::spawn(move || loop {
            if let Ok(msg) = t_rx.recv() {
                match msg {
                    Message::Open { server, tx } => {
                        let c_id = servers.get_next_id();

                        servers.update(c_id, server);
                        println!(
                            "{}: {} active servers (new with id {})",
                            i,
                            servers.len(),
                            c_id
                        );

                        let _ = tx.send(c_id);
                    }
                    Message::Close { id, code } => {
                        servers.empty(id);

                        println!("{}: {} active servers ({:?})", i, servers.len(), code);
                    }
                    Message::Login {
                        id,
                        username,
                        password,
                        tx,
                    } => {
                        let status = {
                            if let Some(user) = &users.get_by_name(&username) {
                                match pbkdf2::pbkdf2_check(&password, &user.password) {
                                    Ok(()) => {
                                        if let Some(mut server) = servers.get(id) {
                                            *server.user_id.write() = Some(user.id);
                                            servers.update(id, server);
                                        }

                                        true
                                    }
                                    _ => false,
                                }
                            } else {
                                false
                            }
                        };

                        let _ = tx.send(JsonMessage::LoginResponse { status });
                    }
                    Message::Register {
                        id,
                        username,
                        password,
                        tx,
                    } => {
                        let status = {
                            if users.contains_username(&username) {
                                false
                            } else {
                                let user_id = users.add(&username, &password);

                                if let Some(mut server) = servers.get(id) {
                                    *server.user_id.write() = Some(user_id);
                                    servers.update(id, server);
                                }

                                true
                            }
                        };

                        let _ = tx.send(JsonMessage::RegisterResponse { status });
                    }
                    Message::Message { user_id, msg } => {
                        if let Some(user) = &users.get_by_id(user_id) {
                            if let Ok(message) = serde_json::to_string(&JsonMessage::Message {
                                username: user.name.clone(),
                                msg: msg.clone(),
                            }) {
                                servers.read().for_each(|_, servers| {
                                    if let Some(server) = servers.first() {
                                        if let Some(user_id_other) = *server.user_id.read() {
                                            if users.in_range(user_id, user_id_other) {
                                                let _ = server.socket.send(message.clone());
                                            }
                                        }
                                    }
                                });
                            }
                        }
                    }
                    Message::Location { user_id, lat, lon } => {
                        if let Some(ref mut user) = users.get_mut_by_id(user_id) {
                            user.lat = lat;
                            user.lon = lon;
                        }
                    }
                }
            } else {
                thread::yield_now();
            }
        }));
    }

    threads.push(thread::spawn(move || {
        while let Ok(msg) = rx.recv() {
            let _ = t_tx.send(msg);
        }
    }));

    for thread in threads {
        let _ = thread.join();
    }
}
