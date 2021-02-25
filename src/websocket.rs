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
    Connect(WsConnectRequest)
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all="camelCase")]
struct WsConnectRequest {
    lobby_id: String,
    #[serde(with = "hex")]
    token: Token,
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


    let messages = UnboundedReceiverStream::new(rx).map(|text| {
        Ok(warp::ws::Message::text(text))
    });
    tokio::task::spawn(messages.forward(ws_tx).map(|res| {
        if let Err(e) = res {
            eprintln!("websocket send error: {}", e);
        }
    }));

    let mut connection_players = Vec::new();
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

        match client_msg {
            WsClientMessage::Connect(req) => {
                let mut lobby_mgr = mgr.lock().unwrap();
                let (lobby_data, player_data) = {
                    let lobby = match lobby_mgr.lobbies.get_mut(&req.lobby_id) {
                        None => continue,
                        Some(lobby) => lobby,
                    };
    
                    let player_data = match lobby.token_player.get(&req.token) {
                        None => continue,
                        Some(&player_id) => {
                            connection_players.push((req.lobby_id.clone(), player_id));
                            let player = lobby.players.get_mut(&player_id).unwrap();
                            player.connection_count += 1;

                            if player.connection_count == 1 {
                                // update required
                                Some(StrippedPlayer::from(player.clone()))
                            } else {
                                None
                            }
                        }
                    };
    
    
                    let lobby_data = StrippedLobby::from(lobby.clone());
                    (lobby_data, player_data)
                };

                if let Some(data) = player_data {
                    lobby_mgr.send_update(&req.lobby_id, LobbyEvent::PlayerData(data));
                }

                // update current state to prevent desync
                let resp = serde_json::to_string(&LobbyEvent::LobbyState(lobby_data)).unwrap();
                tx.send(resp).unwrap();

                // add connection to connection pool
                lobby_mgr.websocket_connections
                    .entry(req.lobby_id)
                    .or_insert_with(|| Vec::new())
                    .push(WsConnection {
                        _conn_id: conn_id,
                        tx: tx.clone(),
                    });
            }
        }
    }
    // connection done, disconnect!
    let mut lobby_mgr = mgr.lock().unwrap();
    for (lobby_id, player_id) in connection_players.into_iter() {
        let update = match lobby_mgr.lobbies.get_mut(&lobby_id) {
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