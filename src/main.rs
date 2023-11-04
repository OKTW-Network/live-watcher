use std::{
    collections::HashMap,
    env,
    error::Error,
    net::SocketAddr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use futures::{
    channel::mpsc::{unbounded, UnboundedSender},
    future, pin_mut, StreamExt, TryStreamExt,
};
use log::{error, info};
use mimalloc::MiMalloc;
use parking_lot::{Mutex, RwLock};
use serde_json::{json, Value};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

type Channels = RwLock<HashMap<String, RwLock<HashMap<SocketAddr, Arc<Watcher>>>>>;

#[derive(Default)]
struct State {
    channels: Channels,
    counter: AtomicUsize,
}

struct Watcher {
    name: RwLock<String>,
    channel: Mutex<Option<String>>,
    uuid: usize,
    tx: UnboundedSender<Message>,
}

impl State {
    fn add_watcher(&self, channel: &str, addr: SocketAddr, watcher: Arc<Watcher>) {
        let channels = self.channels.read();
        if let Some(channel) = channels.get(channel) {
            channel.write().insert(addr, watcher.clone());
        } else {
            drop(channels); // Release read lock
            self.channels
                .write()
                .entry(channel.to_string())
                .or_default()
                .write()
                .insert(addr, watcher.clone());
        }

        watcher.set_channel(channel.to_string());
        self.update_viewer_count(channel);
    }

    fn remove_watcher<T: AsRef<str>>(&self, channel: T, addr: &SocketAddr) {
        let channel = channel.as_ref();
        let mut is_empty = false;
        if let Some(mut watchers) = self.channels.read().get(channel).map(|w| w.write()) {
            watchers.remove(addr);

            if watchers.is_empty() {
                is_empty = true;
            }
        }

        if is_empty {
            // Recheck to avoid race condition
            let mut channels = self.channels.write();
            if channels
                .get(channel)
                .map(|w| w.read().is_empty())
                .unwrap_or(false)
            {
                channels.remove(channel);
            }
        }

        self.update_viewer_count(channel);
    }

    fn update_viewer_count(&self, channel: &str) {
        if let Some(watchers) = self.channels.read().get(channel).map(|w| w.read()) {
            for (_, watcher) in watchers.iter() {
                let _ = watcher.tx.unbounded_send(Message::Text(
                    json!({
                            "type": "channelData",
                            "name": watcher.name.read().as_str(),
                            "uuid": watcher.uuid,
                            "nowViewerCount": watchers.len()
                    })
                    .to_string(),
                ));
            }
        }
    }

    fn broadcast_message(&self, sender: &Watcher, message: &str) {
        if let Some(channel) = sender.channel.lock().as_ref() {
            if let Some(watchers) = self.channels.read().get(channel).map(|w| w.read()) {
                let message = Message::Text(
                    json!({
                            "type": "bulletScreenMessage",
                            "msg": message,
                            "sentFrom": sender.name.read().as_str(),
                            "uuid": sender.uuid
                    })
                    .to_string(),
                );
                for (_, watcher) in watchers.iter() {
                    let _ = watcher.tx.unbounded_send(message.clone());
                }
            }
        }
    }

    pub fn next_uuid(&self) -> usize {
        self.counter.fetch_add(1, Ordering::Relaxed)
    }
}

impl Watcher {
    fn new(uuid: usize ,tx: UnboundedSender<Message>) -> Watcher {
        Watcher {
            name: RwLock::default(),
            channel: Mutex::default(),
            uuid,
            tx,
        }
    }

    fn set_name(&self, name: String) {
        *self.name.write() = name;
    }

    fn set_channel(&self, channel: String) {
        *self.channel.lock() = Some(channel);
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    env_logger::init();
    let addr = env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:2362".to_string());

    let listener = TcpListener::bind(&addr).await.expect("Failed to bind");
    info!("Listening on: {}", addr);

    let state = Arc::new(State::default());
    while let Ok((stream, addr)) = listener.accept().await {
        tokio::spawn(handle_connection(state.clone(), addr, stream));
    }

    Ok(())
}

async fn handle_connection(state: Arc<State>, addr: SocketAddr, stream: TcpStream) {
    let ws_stream = match tokio_tungstenite::accept_async(stream).await {
        Ok(ws) => ws,
        Err(err) => return error!("WebSocket handshake failed. {}", err),
    };

    info!("New WebSocket connection: {}", addr);

    let (tx, rx) = unbounded();
    let (outgoing, incoming) = ws_stream.split();
    let me = Arc::new(Watcher::new(state.next_uuid(), tx));
    let incoming_loop = incoming.try_for_each(|msg| {
        if !msg.is_text() {
            return future::ok(());
        }
        let message = msg.to_text().unwrap();
        info!("Received a message: {}", message);

        let data: Value = match serde_json::from_str(message) {
            Ok(data) => data,
            Err(err) => {
                error!("Message parser failed. {}", err);
                return future::ok(());
            }
        };

        match data["method"].as_str().unwrap_or_default() {
            "setName" => {
                if let Some(name) = data["name"].as_str() {
                    me.set_name(name.to_string());
                }
            }
            "joinChannel" => {
                if let Some(new) = data["channelName"].as_str() {
                    if let Some(old) = me.channel.lock().as_ref() {
                        state.remove_watcher(old, &addr);
                    }

                    state.add_watcher(new, addr, me.clone());
                }
            }
            "sendBulletMessage" => {
                if let Some(message) = data["msg"].as_str() {
                    state.broadcast_message(&me, message);
                }
            }
            _ => (),
        };

        future::ok(())
    });

    let outgoing_loop = rx.map(Ok).forward(outgoing);

    // looping
    pin_mut!(incoming_loop, outgoing_loop);
    future::select(incoming_loop, outgoing_loop).await;

    info!("{} disconnected", &addr);

    // Remove self
    if let Some(channel) = me.channel.lock().as_ref() {
        state.remove_watcher(channel, &addr);
    };
}
