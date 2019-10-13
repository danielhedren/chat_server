#![warn(unused_extern_crates)]

extern crate chashmap;
extern crate crossbeam;
extern crate parking_lot;
extern crate pbkdf2;
extern crate serde;
extern crate serde_json;
extern crate ws;

use crossbeam::channel::unbounded;
use parking_lot::RwLock;
use std::{sync::Arc, thread};

mod server;
use server::{JsonMessage, Message, Server, Servers, Users};

const ENDPOINT: &str = "127.0.0.1:3012";
const WORKERS: usize = 4;
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
                                        if let Some(server) = servers.get(id) {
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

                                if let Some(server) = servers.get(id) {
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
