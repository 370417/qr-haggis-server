//! This server's job is to relay messages between pairs of clients over
//! websockets. Each client should send an 8-byte id as their first message,
//! then all remaining binary messages should represent game states (ie qr
//! codes).

use futures::{stream::SplitSink, stream::SplitStream, SinkExt, StreamExt};
use std::env;
use std::sync::Arc;
use std::{collections::HashMap, convert::TryInto};
use tokio::sync::Mutex;
use warp::{
    ws::{Message, WebSocket},
    Filter,
};

/// The id of a client corresponds to an initial game state, represented as
/// the perspective player's hand (compressed) followed by the opponent's hand.
#[derive(Hash, Eq, PartialEq, Copy, Clone)]
struct ClientId([u8; 4], [u8; 4]);

/// Swapping the two halves of the ClientId gives you the id of the opponent's
/// client.
impl ClientId {
    fn swap(&self) -> Self {
        Self(self.1, self.0)
    }
}

/// Stores the sinks of each client so that we can send a message to any client.
#[derive(Clone)]
struct Clients(Arc<Mutex<HashMap<ClientId, Client>>>);
type Client = SplitSink<WebSocket, Message>;

impl Clients {
    /// Add a client's sink, overriding any previous client with the same id.
    /// This means the server does not support multiple clients connecting to
    /// the same game as the same player.
    async fn add(&mut self, client_id: ClientId, client_sink: SplitSink<WebSocket, Message>) {
        self.0.lock().await.insert(client_id, client_sink);
    }

    /// Remove a client and try to close the connection gracefully.
    async fn remove_and_close(&mut self, client_id: &ClientId) {
        if let Some(mut sink) = self.0.lock().await.remove(client_id) {
            if let Err(error) = sink.close().await {
                eprintln!("error closing ws: {}", error);
            }
        }
    }
}

#[tokio::main]
async fn main() {
    let clients = Clients(Arc::new(Mutex::new(HashMap::new())));

    let route = warp::path("v1")
        .and(warp::ws())
        .map(move |ws: warp::ws::Ws| {
            let clients = clients.clone();
            ws.on_upgrade(move |websocket| ws_handler(websocket, clients))
        });

    println!("Running on port {}", port());

    warp::serve(route).run(([127, 0, 0, 1], port())).await;
}

/// Get the port from the $PORT environment variable or use 8080 as the default
fn port() -> u16 {
    match env::var("PORT").map(|str| u16::from_str_radix(&str, 10)) {
        Ok(Ok(port)) => port,
        _ => 8080,
    }
}

/// Read and handle all the messages from a websocket until it closes
async fn ws_handler(mut websocket: WebSocket, mut clients: Clients) {
    let client_id = match read_client_id(&mut websocket).await {
        Some(client_id) => client_id,
        None => return,
    };
    let (sink, stream) = websocket.split();
    clients.add(client_id, sink).await;
    relay_messages(stream, client_id.swap(), &clients).await;
    clients.remove_and_close(&client_id).await;
}

/// Read messages from the websocket until we find a message that can be a
/// ClientId or the socket closes/errors.
///
/// Looping instead of just looking at the first message accounts for pings.
async fn read_client_id(websocket: &mut WebSocket) -> Option<ClientId> {
    loop {
        let message = match websocket.next().await {
            Some(Ok(message)) => message,
            Some(Err(error)) => {
                eprintln!("error receiving ws message: {}", error);
                break None;
            }
            _ => break None,
        };

        if message.is_close() {
            if let Err(error) = websocket.close().await {
                eprintln!("error closing ws: {}", error);
            }
            break None;
        }

        if message.is_binary() {
            let bytes = message.as_bytes();
            if bytes.len() < 8 {
                continue;
            }
            break Some(ClientId(
                bytes[0..4].try_into().unwrap(),
                bytes[4..8].try_into().unwrap(),
            ));
        }
    }
}

/// While the websocket remains open, send all messages received to the
/// opponent's client
async fn relay_messages(
    mut client_stream: SplitStream<WebSocket>,
    opponent_id: ClientId,
    clients: &Clients,
) {
    while let Some(result) = client_stream.next().await {
        match result {
            Ok(message) => {
                if message.is_close() {
                    break;
                } else if message.is_binary() {
                    relay_message(opponent_id, message.as_bytes(), &clients).await;
                }
            }
            Err(error) => eprintln!("error receiving ws message: {}", error),
        }
    }
}

/// Send a message to an opponent's client if the client exists
async fn relay_message(opponent_id: ClientId, message: &[u8], clients: &Clients) {
    let mut clients = clients.0.lock().await;
    let opponent = match clients.get_mut(&opponent_id) {
        Some(opponent) => opponent,
        None => return,
    };

    let result = opponent.send(Message::binary(message)).await;

    if let Err(error) = result {
        eprintln!("error sending ws message: {}", error);
    }
}
