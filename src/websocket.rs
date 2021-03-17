use std::sync::{Arc, Mutex, atomic::{AtomicUsize, Ordering}};

use crate::{LobbyData, LobbyEvent, LobbyManager, MatchLogEvent, StrippedPlayer, game_manager::MatchData};
use futures::{StreamExt, future};
use futures::FutureExt;
use mozaic_core::Token;
use serde::{Serialize, Deserialize};
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use warp::ws::WebSocket;

pub struct WsConnection {
    #[warn(dead_code)]
    _conn_id: usize,
    tx: mpsc::UnboundedSender<String>,
}

impl WsConnection {
    pub fn send(&mut self, msg: String) -> Result<(), ()> {
        self.tx.send(msg).map_err(|_| ())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type")]
#[serde(rename_all="camelCase")]
enum WsClientMessage {
    AuthenticatePlayer(AuthenticatePlayer),
    SubscribeToLobby(SubscribeToLobby),
    SubscribeToMatch(SubscribeToMatch),
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

#[derive(Serialize, Deserialize)]
#[serde(rename_all="camelCase")]
struct SubscribeToMatch {
    match_id: String,
    stream_id: usize,
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
            WsClientMessage::SubscribeToMatch(req) => self.subscribe_to_match(req),
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
            LobbyData::from(lobby.clone())
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

    fn subscribe_to_match(&mut self, req: SubscribeToMatch) {
        let lobby_mgr = self.mgr.lock().unwrap();
        let game_mgr = lobby_mgr.game_manager.lock().unwrap();
        // TODO: what if this does not match? Should a completed match log be translated back?
        if let Some(MatchData::InProgress { log_stream }) = game_mgr.get_match_data(&req.match_id) {
            let tx = self.tx.clone();
            let task = log_stream.reader().for_each(move |log_entry| {
                let event= LobbyEvent::MatchLogEvent(MatchLogEvent {
                    stream_id: req.stream_id,
                    event: log_entry.as_ref().clone(),
                });
                let serialized = serde_json::to_string(&event).unwrap();
                // just carry on if things are broken, for now.
                // TODO: have some decency
                let _result = tx.send(serialized);
                future::ready(())
            });
            tokio::spawn(task);
        }
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
                        // It should not happen that this condition is false, but it does,
                        // and it crashes the server. Please help.
                        if player.connection_count > 0 {
                            player.connection_count -= 1;
                        }

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