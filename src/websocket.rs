use std::sync::{Arc, Mutex, atomic::{AtomicUsize, Ordering}};

use crate::{LobbyEvent, LobbyManager, StrippedLobby, StrippedPlayer, WsConnection};
use futures::StreamExt;
use futures::FutureExt;
use mozaic_core::Token;
use serde::{Serialize, Deserialize};
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use warp::ws::WebSocket;


#[derive(Serialize, Deserialize)]
#[serde(tag = "type")]
#[serde(rename_all="camelCase")]
enum WsClientMessage {
    AuthenticatePlayer(AuthenticatePlayer),
    SubscribeToLobby(SubscribeToLobby),
}


#[derive(Serialize, Deserialize)]
#[serde(rename_all="camelCase")]
struct AuthenticatePlayer {
    lobby_id: String,
    #[serde(with = "hex")]
    token: Token,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all="camelCase")]
struct SubscribeToLobby {
    lobby_id: String,
}

struct ConnectionHandler {
    conn_id: usize,
    mgr: Arc<Mutex<LobbyManager>>,
    // (lobby_id, player_id)
    authenticated_players: Vec<(String, usize)>,

    tx: mpsc::UnboundedSender<String>,

}

impl ConnectionHandler {

    fn handle_message(&mut self, msg: WsClientMessage) {
        match msg {
            WsClientMessage::AuthenticatePlayer(req) => self.authenticate(req),
            WsClientMessage::SubscribeToLobby(req) => self.subscribe_to_lobby(req),
        }
    }

    fn subscribe_to_lobby(&mut self, req: SubscribeToLobby) {
        let mut lobby_mgr = self.mgr.lock().unwrap();

        let lobby_data = {
            // TODO: log, error, ANYTHING
            let lobby = match lobby_mgr.lobbies.get_mut(&req.lobby_id) {
                None => return,
                Some(lobby) => lobby,
            };
            StrippedLobby::from(lobby.clone())
        };


        // update current state to prevent desync
        let resp = serde_json::to_string(&LobbyEvent::LobbyState(lobby_data)).unwrap();
        self.tx.send(resp).unwrap();

        // add connection to connection pool
        lobby_mgr.websocket_connections
            .entry(req.lobby_id)
            .or_insert_with(|| Vec::new())
            .push(WsConnection {
                _conn_id: self.conn_id,
                tx: self.tx.clone(),
            });
    }

    fn authenticate(&mut self, req: AuthenticatePlayer) {
        let mut lobby_mgr = self.mgr.lock().unwrap();
        let player_data = {
            // TODO: log, error, ANYTHING
            let lobby = match lobby_mgr.lobbies.get_mut(&req.lobby_id) {
                None => return,
                Some(lobby) => lobby,
            };

            // TODO: log, error, ANYTHING

            match lobby.token_player.get(&req.token) {
                None => return,
                Some(&player_id) => {
                    self.authenticated_players.push((req.lobby_id.clone(), player_id));
                    let player = lobby.players.get_mut(&player_id).unwrap();
                    player.connection_count += 1;

                    if player.connection_count == 1 {
                        // update required
                        Some(StrippedPlayer::from(player.clone()))
                    } else {
                        None
                    }
                }
            }
        };

        if let Some(data) = player_data {
            lobby_mgr.send_update(&req.lobby_id, LobbyEvent::PlayerData(data));
        }
    }

    fn disconnect(&mut self) {
        let mut lobby_mgr = self.mgr.lock().unwrap();
        for (lobby_id, player_id) in self.authenticated_players.iter() {
            let update = match lobby_mgr.lobbies.get_mut(lobby_id) {
                None => continue,
                Some(lobby) => {
                    lobby.players.get_mut(&player_id).and_then(|player| {
                        player.connection_count -= 1;
                        if player.connection_count == 0 {
                            // update required
                            Some(StrippedPlayer::from(player.clone()))
                        } else {
                            None
                        }
                    })
                }
            };
    
            if let Some(player_data) = update {
                lobby_mgr.send_update(&lobby_id, LobbyEvent::PlayerData(player_data));
            }
        }
    }
}

static NEXT_CONNECTION_ID: AtomicUsize = AtomicUsize::new(1);

pub async fn handle_websocket(
    ws: WebSocket,
    mgr: Arc<Mutex<LobbyManager>>
)
{
    let conn_id = NEXT_CONNECTION_ID.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = mpsc::unbounded_channel();
    let (ws_tx, mut ws_rx) = ws.split();

    let mut handler = ConnectionHandler {
        conn_id,
        mgr,
        tx,
        authenticated_players: Vec::new(),
    };

    let messages = UnboundedReceiverStream::new(rx).map(|text| {
        Ok(warp::ws::Message::text(text))
    });
    tokio::task::spawn(messages.forward(ws_tx).map(|res| {
        if let Err(e) = res {
            eprintln!("websocket send error: {}", e);
        }
    }));

    while let Some(res) = ws_rx.next().await {
        let ws_msg = match res {
            Ok(ws_msg) => ws_msg,
            Err(e) => {
                eprintln!("websocket error(uid={}): {}", conn_id, e);
                break;
            }
        };

        // TODO: UGLY UGh
        let client_msg: WsClientMessage = match ws_msg.to_str() {
            Ok(text) => match serde_json::from_str(text) {
                Ok(req) => req,
                Err(err) => {
                    eprintln!("{}", err);
                    continue
                }
            }
            Err(()) => continue,
        };

        handler.handle_message(client_msg);
    }
    // connection done, disconnect!
    handler.disconnect();
}