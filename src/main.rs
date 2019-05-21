extern crate ws;
extern crate serde;
extern crate serde_json;
extern crate pbkdf2;

use std::{sync::mpsc, sync::atomic, sync::Arc, thread, collections::HashMap, time::Duration, io};
use ws::{listen, CloseCode, Handler, Handshake, Result};
use serde::{Deserialize, Serialize};

const PBKDF2_ITERATIONS: u32 = 1;
const ENDPOINT: &str = "127.0.0.1:3012";

#[derive(Serialize, Deserialize)]
enum JsonMessage {
    Location { lat: f32, lon: f32},
    Login { username: String, password: String },
    LoginResponse { status: bool },
    Register { username: String, password: String },
    RegisterResponse { status: bool },
    SendMessage { msg: String },
    Message { username: String, msg: String },
    Error { reason: String }
}

enum Message {
    Open { server: Server, tx: mpsc::Sender<usize> },
    Close { id: usize, code: ws::CloseCode },
    Login { id: usize, username: String, password: String, tx: mpsc::Sender<JsonMessage> },
    Register { id: usize, username: String, password: String, tx: mpsc::Sender<JsonMessage> },
    Message { id: usize, msg: String },
    Location { id: usize, lat: f32, lon: f32 },
}

struct User {
    id: usize,
    name: String,
    lat: f32,
    lon: f32,
    password: String
}

impl User {
    fn new(id: usize, name: String, password: String) -> User {
        User { id, name, lat: 0.0, lon: 0.0, password }
    }
}

impl User {
    fn distance_to(&self, other: &User) -> f32 {
        let mut ph1: f32 = self.lat - other.lat;
        let mut th1: f32 = self.lon;
        let mut th2:f32 = other.lon;

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

struct Users {
    current_id: usize,
    users: HashMap<usize, User>,
    users_by_name: HashMap<String, usize>
}

impl Users {
    fn new() -> Users {
        Users { current_id: 0, users: HashMap::new(), users_by_name: HashMap::new() }
    }

    fn contains_id(&self, id: &usize) -> bool {
        self.users.contains_key(id)
    }

    fn contains_username(&self, username: &str) -> bool {
        self.users_by_name.contains_key(username)
    }

    fn add(&mut self, username: &str, password: &str) -> usize {
        self.current_id += 1;

        let user = User::new(self.current_id, username.to_string(), pbkdf2::pbkdf2_simple(&password, PBKDF2_ITERATIONS).unwrap());

        self.users.insert(self.current_id, user);
        self.users_by_name.insert(username.to_string(), self.current_id);

        self.current_id
    }

    fn get_by_id(&self, id: &usize) -> Option<&User> {
        self.users.get(id)
    }

    fn get_mut_by_id(&mut self, id: &usize) -> Option<&mut User> {
        self.users.get_mut(id)
    }

    fn get_by_name(&self, username: &str) -> Option<&User> {
        let user_id = self.users_by_name.get(username);
        match user_id {
            Some(user_id) => self.users.get(&user_id),
            None => None
        }
    }

    fn get_mut_by_name(&mut self, username: &str) -> Option<&mut User> {
        let user_id = self.users_by_name.get(username);
        match user_id {
            Some(user_id) => self.users.get_mut(&user_id),
            None => None
        }
    }
}

// Server web application handler
#[derive(Clone)]
struct Server {
    id: usize,
    user_id: Option<usize>,
    socket: ws::Sender,
    channel: mpsc::Sender<Message>,
}

impl Handler for Server {
    fn on_open(&mut self, _shake: Handshake) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        let _ = self.channel.send(Message::Open {server: self.clone(), tx });

        if let Ok(id) = rx.recv() {
            self.id = id;
        }

        Ok(())
    }

    // Handle messages recieved in the websocket (in this case, only on /ws)
    fn on_message(&mut self, msg: ws::Message) -> Result<()> {
        let (tx, rx) = mpsc::channel();

        if let Ok(s) = msg.as_text() {
            if let Ok(val) = serde_json::from_str(s) {
                let val: JsonMessage = val;
                //println!("{}: {}", self.id, serde_json::to_string(&val).unwrap());
                match val {
                    JsonMessage::Location { lat, lon } => {
                        let _ = self.channel.send(Message::Location { id: self.id, lat, lon });
                    },
                    JsonMessage::Login { username, password } => {
                        let _ = self.channel.send(Message::Login { id: self.id, username, password, tx });

                        if let Ok(response) = rx.recv() {
                            if let Ok(json) = serde_json::to_string(&response) {
                                let _ = self.socket.send(json);
                            }
                        }
                    },
                    JsonMessage::Register { username, password } => {
                        let _ = self.channel.send(Message::Register { id: self.id, username, password, tx });

                        if let Ok(response) = rx.recv() {
                            if let Ok(json) = serde_json::to_string(&response) {
                                let _ = self.socket.send(json);
                            }
                        }
                    },
                    JsonMessage::SendMessage { msg } => {
                        let _ = self.channel.send(Message::Message { id: self.id, msg });
                    },
                    _ => ()
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
    let (tx, rx) = mpsc::channel();

    let mut current_server_id: usize = 0;
    let mut servers: HashMap<usize, Server> = HashMap::new();

    let mut users = Users::new();
    
    let server = thread::spawn(move || {
        if let Ok(socket) = ws::Builder::new()
        .with_settings(ws::Settings {max_connections: 10000, ..ws::Settings::default()})
        .build(|out| Server {
            id: 0,
            user_id: None,
            socket: out,
            channel: tx.clone(),
        }) {
            let _ = socket.listen(ENDPOINT);
        }
    });

    while let Ok(msg) = rx.recv() {
        match msg {
            Message::Open { server, tx } => {
                current_server_id += 1;
                servers.insert(current_server_id, server);
                println!("{} active servers", servers.len());
                let _ = tx.send(current_server_id);
            },
            Message::Login { id, username, password, tx } => {
                let status = {
                    if let Some(user) = users.get_by_name(&username) {
                        match pbkdf2::pbkdf2_check(&password, &user.password) {
                            Ok(()) => {
                                if let Some(server) = servers.get_mut(&id) {
                                    server.user_id = Some(user.id);
                                }
                                true
                            },
                            _ => false
                        } 
                    } else {
                        false
                    }
                };

                let _ = tx.send(JsonMessage::LoginResponse { status });
            },
            Message::Register { id, username, password, tx } => {
                let status = {
                    if users.contains_username(&username) {
                        false
                    } else {
                        let user_id = users.add(&username, &password);
                        if let Some(server) = servers.get_mut(&id) {
                            server.user_id = Some(user_id);
                        }
                        true
                    }
                };
                
                let _ = tx.send(JsonMessage::RegisterResponse { status });
            },
            Message::Message { id, msg } => {
                if let Some(server) = servers.get(&id) {
                    if let Some(user_id) = server.user_id {
                        if let Some(user) = &users.get_by_id(&user_id) {
                            if let Ok(message) = serde_json::to_string(&JsonMessage::Message { username: user.name.clone(), msg: msg.clone() }) {
                                for (_, server) in &servers {
                                    if let Some(user_id_other) = server.user_id {
                                        if let Some(user_other) = &users.get_by_id(&user_id_other) {
                                            if user.within_bounds(user_other, 0.1) && user_other.distance_to(user) < 10.0 {
                                                let _ = server.socket.send(message.clone());
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            },
            Message::Close { id, code: _ } => {
                servers.remove(&id);
                println!("{} active servers", servers.len());
            },
            _ => ()
        }
    }

    let _ = server.join();
}
