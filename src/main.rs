#![feature(async_closure)]

use mozaic_core::msg_stream::msg_stream;
use mpsc::UnboundedSender;
use serde::{Deserialize, Serialize};

use mozaic_core::client_manager::ClientHandle;
use futures::{FutureExt, StreamExt, channel::mpsc::SendError, stream};

mod planetwars;

use mozaic_core::{Token, GameServer, MatchCtx};
use mozaic_core::msg_stream::{MsgStreamHandle};
use tokio::sync::mpsc;
use uuid::Uuid;

use std::{convert::Infallible, unimplemented};
use warp::{reply::{json,Reply,Response}, ws::WebSocket};
use warp::Filter;


use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::collections::HashMap;

use hex::FromHex;
use rand::Rng;

#[derive(Serialize, Deserialize, Debug, Clone)]
struct Player {
    name: String,
    #[serde(with = "hex")]
    token: Token,
}

impl Player {
    pub fn authorize_header(&self, authorization: &Option<String>) -> bool {
        if authorization.is_none() {
            false
        } else {
            let bearer_token = authorization.as_ref().unwrap().to_lowercase();
            let token_string = bearer_token.strip_prefix("bearer ");
            if token_string.is_none() {
                return false;
            }
            let token_opt = Token::from_hex(token_string.unwrap());
            if token_opt.is_err() || token_opt.unwrap() != self.token {
                false
            } else {
                true
            }
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
struct StrippedPlayer {
    name: String,
}

struct GameManager {
    game_server: GameServer,
    matches: HashMap<String, MatchData>
}

struct MatchData {
    log: MsgStreamHandle<String>,
}

impl GameManager {
    fn create_match(&mut self, tokens: Vec<Token>, game_config: planetwars::Config) -> String {
        let clients = tokens.iter().map(|token| {
            self.game_server.get_client(&token)
        }).collect::<Vec<_>>();
    
        let match_id = gen_match_id();
        let log = msg_stream();
        self.matches.insert(match_id.clone(),
            MatchData { log: log.clone() }
        );
        println!("Starting match");
        tokio::spawn(run_match(
            clients,
            self.game_server.clone(),
            game_config,
            log)
        );
        return match_id;
    }

    fn list_matches(&self) -> Vec<String> {
        self.matches.keys().cloned().collect()
    }
}

#[derive(Clone, Debug)]
struct Lobby {
    id: String,
    name: String,
    public: bool,
    players: HashMap<String,Player>,
    proposals: HashMap<String, Proposal>,
    // #[serde(with = "hex")]
    lobby_token: Token,
}

#[derive(Serialize, Deserialize, Debug)]
struct StrippedLobby {
    id: String,
    name: String,
    public: bool,
    players: HashMap<String,StrippedPlayer>,
}

#[derive(Serialize, Deserialize, Debug)]
struct LobbyConfig {
    name: String,
    public: bool,
}

#[derive(Serialize, Deserialize, Debug)]
struct MatchStartConfig {
    players: Vec<String>
}

#[derive(Serialize, Deserialize, Debug)]
struct MatchStartResult {
    match_id: String,
}

impl From<LobbyConfig> for Lobby {
    fn from(config: LobbyConfig) -> Lobby {
        let id: [u8; 16] = rand::thread_rng().gen();
        Lobby {
            id: hex::encode(id),
            name: config.name,
            public: config.public,
            players: HashMap::new(),
            lobby_token: rand::thread_rng().gen(),
            proposals: HashMap::new(),
        }
    }
}

impl From<Lobby> for StrippedLobby {
    fn from(lobby: Lobby) -> StrippedLobby {
        StrippedLobby {
            id: lobby.id,
            name: lobby.name,
            public: lobby.public,
            players: lobby.players.iter().map(|(k,v)| (k.clone(),StrippedPlayer::from(v.clone()))).collect(),
        }
    }
}

impl From<Player> for StrippedPlayer {
    fn from(player: Player) -> StrippedPlayer {
        StrippedPlayer {
            name: player.name,
        }
    }
}

impl Lobby {
    pub fn authorize_header(&self, authorization: &Option<String>) -> bool {
        if authorization.is_none() {
            false
        } else {
            let bearer_token = authorization.as_ref().unwrap().to_lowercase();
            let token_string = bearer_token.strip_prefix("bearer ");
            if token_string.is_none() {
                return false;
            }
            let token_opt = Token::from_hex(token_string.unwrap());
            if token_opt.is_err() || token_opt.unwrap() != self.lobby_token {
                false
            } else {
                true
            }
        }
    }
}

struct LobbyManager {
    game_manager: Arc<Mutex<GameManager>>,
    lobbies: HashMap<String, Lobby>,
    websocket_connections: HashMap<String, Vec<WsConnection>>,
}

impl LobbyManager {
    pub fn new(game_manager: Arc<Mutex<GameManager>>) -> Self {
        Self {
            game_manager,
            lobbies: HashMap::new(),
            websocket_connections: HashMap::new(),
        }
    }

    pub fn create_lobby(&mut self, config: LobbyConfig) -> Lobby {
        let lobby: Lobby = config.into();
        self.lobbies.insert(lobby.id.clone(), lobby.clone());
        lobby
    }

    pub fn send_update(&mut self, lobby_id: &str, ev: LobbyEvent) {
        let serialized = serde_json::to_string(&ev).unwrap();
        if let Some(connections) = self.websocket_connections.get_mut(lobby_id) {
            let mut i = 0;
            while i < connections.len() {
                if connections[i].send(serialized.clone()).is_ok() {
                    i += 1;
                } else {
                    // connection dropped, remove it from the list
                    connections.swap_remove(i);
                }
            }
        }
    }
}

struct WsConnection {
    conn_id: usize,
    tx: mpsc::UnboundedSender<String>,
}

impl WsConnection {
    pub fn send(&mut self, msg: String) -> Result<(), ()> {
        self.tx.send(msg).map_err(|_| ())
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
enum AcceptedState {
    Unanswered,
    Accepted,
    Rejected,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct AcceptingPlayer {
    name: String,
    status: AcceptedState,
}

#[derive(Serialize, Deserialize, Debug)]
struct ProposalParams {
    owner: String,
    config: planetwars::Config,
    players: Vec<String>
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct Proposal {
    owner: String,
    config: planetwars::Config,
    players: Vec<AcceptingPlayer>,
    id: String
}

impl From<ProposalParams> for Proposal {
    fn from(params: ProposalParams) -> Proposal {
        Proposal {
            owner: params.owner,
            config: params.config,
            players: params.players.iter().map(|name| {
                AcceptingPlayer {
                    name: name.to_string(),
                    status: AcceptedState::Unanswered
                }
            }).collect(),
            id: Uuid::new_v4().to_hyphenated().to_string(),
        }
    }
}

// TODO
#[derive(Serialize, Deserialize)]
#[serde(tag="type", content = "data")]
#[serde(rename_all="camelCase")]
enum LobbyEvent {
    LobbyState(StrippedLobby),
    PlayerData(StrippedPlayer),
}

async fn run_match(
    mut clients: Vec<ClientHandle>,
    mut serv: GameServer,
    config: planetwars::Config,
    log: MsgStreamHandle<String>)
{
    let event_bus = msg_stream();
    let players = stream::iter(clients.iter_mut().enumerate())
        .then(|(i, client)| {
            let player_token: Token = rand::thread_rng().gen();
            let player_id = (i+1) as u32;
            let player = serv.register_player(player_id, player_token, &event_bus);
            client.run_player(player_token).map(move |_| (player_id, player))
        }).collect().await;
    
    let match_ctx = MatchCtx::new(event_bus, players, log);
    let pw_match = planetwars::PwMatch::create(match_ctx, config);
    pw_match.run().await;
    println!("match done");
}

fn with_game_manager(
    game_manager: Arc<Mutex<GameManager>>,
) -> impl Clone + Filter<Extract = (Arc<Mutex<GameManager>>,), Error = Infallible>
{
    warp::any().map(move || game_manager.clone())
}

fn with_lobby_manager(
    lobby_manager: Arc<Mutex<LobbyManager>>,
) -> impl Clone + Filter<Extract = (Arc<Mutex<LobbyManager>>,), Error = Infallible>
{
    warp::any().map(move || lobby_manager.clone())
}

fn create_lobby(
    mgr: Arc<Mutex<LobbyManager>>,
    lobby_config: LobbyConfig,
) -> impl Reply {
    let mut manager = mgr.lock().unwrap();
    let lobby = manager.create_lobby(lobby_config);
    json(&StrippedLobby::from(lobby.clone()))
}

fn get_lobbies(
    mgr: Arc<Mutex<LobbyManager>>,
) -> impl Reply {
    let manager = mgr.lock().unwrap();
    return json(&manager.lobbies.values().filter_map(|lobby| {
        if lobby.public {
            Some((*lobby).clone().into())
        } else {
            None
        }
    }).collect::<Vec<StrippedLobby>>());
}

fn get_lobby_by_id(
    id: String,
    mgr: Arc<Mutex<LobbyManager>>,
) -> Response {
    let manager = mgr.lock().unwrap();
    match manager.lobbies.get(&id.to_lowercase()) {
        Some(lobby) => {
            json(&StrippedLobby::from(lobby.clone())).into_response()
        },
        None => warp::http::StatusCode::NOT_FOUND.into_response()
    }
}

fn update_lobby_by_id(
    id: String,
    mgr: Arc<Mutex<LobbyManager>>,
    authorization: Option<String>,
    lobby_conf: LobbyConfig,
) -> Response {
    let mut manager = mgr.lock().unwrap();
    match manager.lobbies.get(&id.to_lowercase()) {
        Some(lobby) => {
            if lobby.authorize_header(&authorization) {
                let mut new_lobby = lobby.clone();
                new_lobby.name = lobby_conf.name;
                new_lobby.public = lobby_conf.public;
                manager.lobbies.insert(new_lobby.id.to_lowercase(), new_lobby);
                return warp::http::StatusCode::OK.into_response();
            } else {
                return warp::http::StatusCode::UNAUTHORIZED.into_response();
            }
        },
        None => warp::http::StatusCode::NOT_FOUND.into_response()
    }
}

fn delete_lobby_by_id(
    id: String,
    mgr: Arc<Mutex<LobbyManager>>,
    authorization: Option<String>,
) -> Response {
    let mut manager = mgr.lock().unwrap();
    match manager.lobbies.get(&id.to_lowercase()) {
        Some(lobby) => {
            if lobby.authorize_header(&authorization) {
                manager.lobbies.remove(&id);
                return warp::http::StatusCode::OK.into_response();
            } else {
                return warp::http::StatusCode::UNAUTHORIZED.into_response();
            }
        },
        None => warp::http::StatusCode::NOT_FOUND.into_response()
    }
}

fn add_player_to_lobby(
    id: String,
    mgr: Arc<Mutex<LobbyManager>>,
    player: Player,
) -> Response {
    let mut manager = mgr.lock().unwrap();
    // TODO: translate this in a filter maybe?
    let lobby_id = id.to_lowercase();


    match manager.lobbies.get_mut(&lobby_id) {
        None => return warp::http::StatusCode::NOT_FOUND.into_response(),
        Some(lobby) => {
            // TODO: we should check whether the name is available
            lobby.players.insert(player.name.clone(), player.clone());
        },
    }

    // update other clients
    manager.send_update(&lobby_id, LobbyEvent::PlayerData(StrippedPlayer::from(player)));

    // TODO: maybe return player?
    return warp::http::StatusCode::OK.into_response();
}

fn update_player_in_lobby(
    id: String,
    name: String,
    mgr: Arc<Mutex<LobbyManager>>,
    authorization: Option<String>,
    player_update: StrippedPlayer,
) -> Response {
    let mut manager = mgr.lock().unwrap();
    match manager.lobbies.get_mut(&id.to_lowercase()) {
        Some(lobby) => {
            match lobby.players.get(&name) {
                Some(player) => {
                    if player.authorize_header(&authorization) {
                        let mut new_player = player.clone();
                        new_player.name = player_update.name;
                        lobby.players.remove(&name);
                        lobby.players.insert(new_player.name.clone(), new_player);
                        warp::http::StatusCode::OK.into_response()
                    } else {
                        warp::http::StatusCode::UNAUTHORIZED.into_response()
                    }
                }
                None => warp::http::StatusCode::NOT_FOUND.into_response()
            }
        },
        None => warp::http::StatusCode::NOT_FOUND.into_response()
    }
}

fn remove_player_from_lobby(
    id: String,
    name: String,
    mgr: Arc<Mutex<LobbyManager>>,
    authorization: Option<String>,
) -> Response {
    let mut manager = mgr.lock().unwrap();
    match manager.lobbies.get_mut(&id.to_lowercase()) {
        Some(lobby) => {
            match lobby.players.get(&name) {
                Some(player) => {
                    if player.authorize_header(&authorization) || lobby.authorize_header(&authorization){
                        lobby.players.remove(&name);
                        warp::http::StatusCode::OK.into_response()
                    } else {
                        warp::http::StatusCode::UNAUTHORIZED.into_response()
                    }
                }
                None => warp::http::StatusCode::NOT_FOUND.into_response()
            }
        },
        None => warp::http::StatusCode::NOT_FOUND.into_response()
    }
}

fn add_proposal_to_lobby(
    id: String,
    mgr: Arc<Mutex<LobbyManager>>,
    authorization: Option<String>,
    params: ProposalParams
) -> Response {
    let mut manager = mgr.lock().unwrap();
    match manager.lobbies.get_mut(&id.to_lowercase()) {
        Some(lobby) => {
            match lobby.players.get(&params.owner) {
                Some(player) => {
                    if player.authorize_header(&authorization) {
                        let proposal: Proposal = params.into();
                        lobby.proposals.insert(proposal.id.clone(), proposal.clone());
                        warp::reply::with_status(
                            json(&proposal),
                            warp::http::StatusCode::OK
                        ).into_response()
                    } else {
                        warp::http::StatusCode::UNAUTHORIZED.into_response()
                    }
                }
                None => warp::http::StatusCode::NOT_FOUND.into_response()
            }
        },
        None => warp::http::StatusCode::NOT_FOUND.into_response()
    }

}

fn start_proposal(
    lobby_id: String,
    proposal_id: String,
    mgr: Arc<Mutex<LobbyManager>>,
    authorization: Option<String>,
) -> Response {
    let mut manager = mgr.lock().unwrap();
    let game_mgr = manager.game_manager.clone();
    let mut game_manager = game_mgr.lock().unwrap();
    match manager.lobbies.get_mut(&lobby_id.to_lowercase()) {
        Some(lobby) => {
            match lobby.proposals.get(&proposal_id) {
                Some(proposal) => {
                    match lobby.players.get(&proposal.owner) {
                        Some(player) => {
                            if player.authorize_header(&authorization) {
                                let mut tokens = vec![];
                                for accepting_player in proposal.players.iter() {
                                    let player_opt = lobby.players.get(&accepting_player.name);
                                    if player_opt.is_none() {
                                        return warp::reply::with_status(
                                            format!("Player {} not found", accepting_player.name),
                                            warp::http::StatusCode::NOT_FOUND
                                        ).into_response();
                                    }
                                    let player = player_opt.unwrap();
                                    if accepting_player.status != AcceptedState::Accepted {
                                        return warp::reply::with_status(
                                            "Not all players are ready",
                                            warp::http::StatusCode::BAD_REQUEST
                                        ).into_response();
                                    }

                                    tokens.push(player.token);
                                }
                                let match_config: planetwars::Config = proposal.config.clone();
                                let match_id = game_manager.create_match(tokens, match_config.clone());
                                return warp::reply::with_status(
                                    json(&MatchStartResult { match_id }),
                                    warp::http::StatusCode::OK
                                ).into_response();
                            } else {
                                warp::http::StatusCode::UNAUTHORIZED.into_response()
                            }
                        }
                        None => {
                            warp::reply::with_status(
                                format!("Proposal owner {} not found", proposal.owner),
                                warp::http::StatusCode::NOT_FOUND
                            ).into_response()
                        }
                    }
                }
                None => {
                    warp::reply::with_status(
                        format!("Proposal {} not found", proposal_id),
                        warp::http::StatusCode::NOT_FOUND
                    ).into_response()
                }
            }
        },
        None => {
            warp::reply::with_status(
                format!("Lobby {} not found", lobby_id),
                warp::http::StatusCode::NOT_FOUND
            ).into_response()
        }
    }
}

fn set_player_accepted_state(
    lobby_id: String,
    proposal_id: String,
    mgr: Arc<Mutex<LobbyManager>>,
    authorization: Option<String>,
    new_status: AcceptingPlayer,
) -> Response {
    let mut manager = mgr.lock().unwrap();
    let game_mgr = manager.game_manager.clone();
    let mut game_manager = game_mgr.lock().unwrap();
    match manager.lobbies.get_mut(&lobby_id.to_lowercase()) {
        Some(lobby) => {
            match lobby.proposals.get_mut(&proposal_id) {
                Some(proposal) => {
                    match lobby.players.get(&new_status.name) {
                        Some(player) => {
                            if player.authorize_header(&authorization) {
                                proposal.players = proposal.players.iter().map(|player| {
                                    if player.name == new_status.name {
                                        AcceptingPlayer {
                                            name: player.name.clone(),
                                            status: new_status.status.clone()
                                        }.clone()
                                    } else {
                                        player.clone()
                                    }
                                }).collect();
                                warp::reply::with_status(
                                    format!(""),
                                    warp::http::StatusCode::OK
                                ).into_response()
                            } else {
                                warp::reply::with_status(
                                    format!("Not authorized to modify ready state for {}", player.name),
                                    warp::http::StatusCode::UNAUTHORIZED
                                ).into_response()
                            }
                        }
                        None => {
                            warp::reply::with_status(
                                format!("Player {} not found", new_status.name),
                                warp::http::StatusCode::NOT_FOUND
                            ).into_response()
                        }
                    }
                }
                None => {
                    warp::reply::with_status(
                        format!("Proposal {} not found", proposal_id),
                        warp::http::StatusCode::NOT_FOUND
                    ).into_response()
                }
            }
        },
        None => {
            warp::reply::with_status(
                format!("Lobby {} not found", lobby_id),
                warp::http::StatusCode::NOT_FOUND
            ).into_response()
        }
    }
}

fn gen_match_id() -> String {
    let id: [u8; 16] = rand::random();
    hex::encode(&id)
}

fn list_matches(
    mgr: Arc<Mutex<GameManager>>
) -> impl Reply
{
    let manager = mgr.lock().unwrap();
    json(&manager.list_matches())
}

fn get_match_log(
    match_id: String,
    mgr: Arc<Mutex<GameManager>>,
) -> warp::reply::Response
{
    let manager = mgr.lock().unwrap();
    match manager.matches.get(&match_id) {
        None => warp::http::StatusCode::NOT_FOUND.into_response(),
        Some(m) => {
            let log = m.log.to_vec().into_iter().map(|e| {
                e.as_ref().to_string()
            }).collect::<Vec<_>>();
            json(&log).into_response()
        }
    }
}

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
    token: String,
}

static NEXT_CONNECTION_ID: AtomicUsize = AtomicUsize::new(1);

async fn handle_websocket(
    ws: WebSocket,
    mgr: Arc<Mutex<LobbyManager>>
)
{
    let conn_id = NEXT_CONNECTION_ID.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = mpsc::unbounded_channel();
    let (ws_tx, mut ws_rx) = ws.split();

    let messages = rx.map(|text| {
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
                return;
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

                lobby_mgr.lobbies.get(&req.lobby_id).map(|lobby| {
                    let lobby_data = LobbyEvent::LobbyState(StrippedLobby::from(lobby.clone()));
                    let resp = serde_json::to_string(&lobby_data).unwrap();
                    tx.send(resp).unwrap();
                }).map(|_| {
                    lobby_mgr.websocket_connections
                        .entry(req.lobby_id)
                        .or_insert_with(|| Vec::new())
                        .push(WsConnection {
                            conn_id,
                            tx: tx.clone(),
                        });
                });
            }
        }
    }
}

#[tokio::main]
async fn main() {
    let game_server = GameServer::new();
    // TODO: can we run these on the same port? Would that be desirable?
    tokio::spawn(game_server.run_ws_server("127.0.0.1:8080".to_string()));

    let game_manager = Arc::new(Mutex::new(GameManager {
        game_server,
        matches: HashMap::new(),
    }));
    let lobby_manager = Arc::new(Mutex::new(LobbyManager::new(game_manager.clone())));

    // POST /lobbies
    let post_lobbies_route = warp::path("lobbies")
        .and(warp::path::end())
        .and(warp::post())
        .and(with_lobby_manager(lobby_manager.clone()))
        .and(warp::body::json())
        .map(create_lobby);

    // GET /lobbies
    let get_lobbies_route = warp::path("lobbies")
        .and(warp::path::end())
        .and(warp::get())
        .and(with_lobby_manager(lobby_manager.clone()))
        .map(get_lobbies);

    // GET /lobbies/<id>
    let get_lobbies_id_route = warp::path!("lobbies" / String)
        .and(warp::path::end())
        .and(warp::get())
        .and(with_lobby_manager(lobby_manager.clone()))
        .map(get_lobby_by_id);

    // PUT /lobbies/<id>
    let put_lobbies_id_route = warp::path!("lobbies" / String)
        .and(warp::path::end())
        .and(warp::put())
        .and(with_lobby_manager(lobby_manager.clone()))
        .and(warp::header::optional::<String>("authorization"))
        .and(warp::body::json())
        .map(update_lobby_by_id);

    // DELETE /lobbies/<id>
    let delete_lobbies_id_route = warp::path!("lobbies" / String)
        .and(warp::path::end())
        .and(warp::delete())
        .and(with_lobby_manager(lobby_manager.clone()))
        .and(warp::header::optional::<String>("authorization"))
        .map(delete_lobby_by_id);

    // POST /lobbies/<id>/join
    let post_lobbies_id_players_route = warp::path!("lobbies" / String / "join")
        .and(warp::path::end())
        .and(warp::post())
        .and(with_lobby_manager(lobby_manager.clone()))
        .and(warp::body::json())
        .map(add_player_to_lobby);

    // PUT /lobbies/<id>/players
    let put_lobbies_id_players_route = warp::path!("lobbies" / String / "players" / String)
        .and(warp::path::end())
        .and(warp::put())
        .and(with_lobby_manager(lobby_manager.clone()))
        .and(warp::header::optional::<String>("authorization"))
        .and(warp::body::json())
        .map(update_player_in_lobby);

    // DELETE /lobbies/<id>/players
    let delete_lobbies_id_players_route = warp::path!("lobbies" / String / "players" / String)
        .and(warp::path::end())
        .and(warp::delete())
        .and(with_lobby_manager(lobby_manager.clone()))
        .and(warp::header::optional::<String>("authorization"))
        .map(remove_player_from_lobby);

    // POST /lobbies/<id>/proposals
    let post_lobbies_id_proposals_route = warp::path!("lobbies" / String / "proposals")
        .and(warp::path::end())
        .and(warp::post())
        .and(with_lobby_manager(lobby_manager.clone()))
        .and(warp::header::optional::<String>("authorization"))
        .and(warp::body::json())
        .map(add_proposal_to_lobby);
    
    // POST /lobbies/<lobby_id>/proposals/<proposal_id>/start
    let post_lobbies_id_proposals_id_start_route = warp::path!("lobbies" / String / "proposals" / String / "start")
        .and(warp::path::end())
        .and(warp::post())
        .and(with_lobby_manager(lobby_manager.clone()))
        .and(warp::header::optional::<String>("authorization"))
        .map(start_proposal);
    
    // POST /lobbies/<lobby_id>/proposals/<proposal_id>/accept
    let post_lobbies_id_proposals_id_accept_route = warp::path!("lobbies" / String / "proposals" / String / "accept")
        .and(warp::path::end())
        .and(warp::post())
        .and(with_lobby_manager(lobby_manager.clone()))
        .and(warp::header::optional::<String>("authorization"))
        .and(warp::body::json())
        .map(set_player_accepted_state);
    
    // GET /matches
    let get_matches_route = warp::path("matches")
        .and(warp::path::end())
        .and(warp::get())
        .and(with_game_manager(game_manager.clone()))
        .map(list_matches);
    
    // GET /matches/<id>
    let get_match_route = warp::path!("matches" / String)
        .and(warp::path::end())
        .and(warp::get())
        .and(with_game_manager(game_manager.clone()))
        .map(get_match_log);

    let websocket_route = warp::path("websocket")
        .and(warp::path::end())
        .and(warp::ws())
        .and(with_lobby_manager(lobby_manager.clone()))
        .map(|ws: warp::ws::Ws, mgr| {
            ws.on_upgrade(move |socket| handle_websocket(socket, mgr))
        });

    let routes = post_lobbies_route
                              .or(get_lobbies_id_route)
                              .or(get_lobbies_route)
                              .or(put_lobbies_id_route)
                              .or(delete_lobbies_id_route)
                              .or(post_lobbies_id_players_route)
                              .or(put_lobbies_id_players_route)
                              .or(delete_lobbies_id_players_route)
                              .or(post_lobbies_id_proposals_id_start_route)
                              .or(post_lobbies_id_proposals_route)
                              .or(post_lobbies_id_proposals_id_accept_route)
                              .or(get_matches_route)
                              .or(get_match_route)
                              .or(websocket_route);
    
    warp::serve(routes).run(([127, 0, 0, 1], 7412)).await;
}
