#![feature(async_closure)]

use chrono::{DateTime, Utc};
use mozaic_core::{EventBus, match_context::PlayerHandle, msg_stream::msg_stream};
use serde::{Deserialize, Serialize};

use mozaic_core::client_manager::ClientHandle;
use futures::{FutureExt, StreamExt, stream};

mod planetwars;

use mozaic_core::{Token, GameServer, MatchCtx};
use mozaic_core::msg_stream::{MsgStreamHandle};
use tokio::sync::mpsc;
use uuid::Uuid;

use std::convert::Infallible;
use warp::{Rejection, reply::{json,Reply,Response}, ws::WebSocket};
use warp::Filter;


use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::collections::{HashMap, HashSet};
use tokio_stream::wrappers::UnboundedReceiverStream;

use hex::FromHex;
use rand::Rng;

#[derive(Debug, Clone)]
struct Player {
    /// Scoped in lobby
    id: usize,
    name: String,
    token: Token,
    connection_count: usize,
    client_connected: bool,
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
struct PlayerParams {
    name: String,
    #[serde(with = "hex")]
    token: Token,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct StrippedPlayer {
    id: usize,
    name: String,
    connected: bool,
    client_connected: bool,
}

struct GameManager {
    game_server: GameServer,
    matches: HashMap<String, MatchData>
}

struct MatchData {
    log: MsgStreamHandle<String>,
}

impl GameManager {
    fn create_match<F>(&mut self, tokens: Vec<Token>, game_config: planetwars::Config, cb: F) -> String
        where F: 'static + Send + Sync + FnOnce(String) -> ()
    {
        let clients = tokens.iter().map(|token| {
            self.game_server.get_client(&token)
        }).collect::<Vec<_>>();
    
        let match_id = gen_match_id();
        let log = msg_stream();
        self.matches.insert(match_id.clone(),
            MatchData { log: log.clone() }
        );
        println!("Starting match {}", &match_id);
        let cb_match_id = match_id.clone();
        tokio::spawn(run_match(
            clients,
            self.game_server.clone(),
            game_config,
            log).map(|_| cb(cb_match_id))
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

    next_player_id: usize,
    // TODO: don't use strings for tokens
    token_player: HashMap<Token, usize>,
    players: HashMap<usize, Player>,

    proposals: HashMap<String, Proposal>,
    matches: HashMap<String, MatchMeta>,
    // #[serde(with = "hex")]
    lobby_token: Token,
}

#[derive(Serialize, Deserialize, Debug)]
struct StrippedLobby {
    id: String,
    name: String,
    public: bool,
    players: HashMap<usize,StrippedPlayer>,
    proposals: HashMap<String, Proposal>,
    matches: HashMap<String, MatchMeta>,
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

impl From<LobbyConfig> for Lobby {
    fn from(config: LobbyConfig) -> Lobby {
        let id: [u8; 16] = rand::thread_rng().gen();
        Lobby {
            id: hex::encode(id),
            name: config.name,
            public: config.public,

            next_player_id: 0,
            players: HashMap::new(),
            token_player: HashMap::new(),

            lobby_token: rand::thread_rng().gen(),
            proposals: HashMap::new(),
            matches: HashMap::new(),
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
            proposals: lobby.proposals.clone(),
            matches: lobby.matches.clone(),
        }
    }
}

impl From<Player> for StrippedPlayer {
    fn from(player: Player) -> StrippedPlayer {
        StrippedPlayer {
            id: player.id,
            name: player.name,
            connected: player.connection_count > 0,
            client_connected: player.client_connected,
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
    /// token -> {(lobby_id, player_id)}
    token_player: HashMap<Token, HashSet<(String, usize)>>,
}

impl LobbyManager {
    pub fn new(game_manager: Arc<Mutex<GameManager>>) -> Self {
        Self {
            game_manager,
            lobbies: HashMap::new(),
            websocket_connections: HashMap::new(),
            token_player: HashMap::new(),
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

fn set_client_connected(lobby_mgr: &Arc<Mutex<LobbyManager>>, token: &Token, value: bool) {
    let mut mgr = lobby_mgr.lock().unwrap();
    let token_players = match mgr.token_player.get(token) {
        None => return,
        Some(players) => players.clone(),
    };
    for (lobby_id, player_id) in token_players {
        let updated = mgr.lobbies.get_mut(&lobby_id).and_then(|lobby| {
            lobby.players.get_mut(&player_id).map(|player| {
                player.client_connected = value;
                StrippedPlayer::from(player.clone())
            })
        });
        if let Some(player_data) = updated {
            mgr.send_update(&lobby_id, LobbyEvent::PlayerData(player_data))
        }
    }
}

fn init_callbacks(mgr_ref: Arc<Mutex<LobbyManager>>) {
    let mgr = mgr_ref.lock().unwrap();
    let mut game_mgr = mgr.game_manager.lock().unwrap();
    let client_mgr = game_mgr.game_server.client_manager_mut();
    
    let cloned_ref = mgr_ref.clone();
    client_mgr.on_connect(Box::new(move |token|
        set_client_connected(&cloned_ref, token, true)));
    let cloned_ref = mgr_ref.clone();
    client_mgr.on_disconnect(Box::new(move |token|
        set_client_connected(&cloned_ref, token, false)));
}

struct WsConnection {
    #[warn(dead_code)]
    _conn_id: usize,
    tx: mpsc::UnboundedSender<String>,
}

impl WsConnection {
    pub fn send(&mut self, msg: String) -> Result<(), ()> {
        self.tx.send(msg).map_err(|_| ())
    }
}

#[derive(Serialize, Deserialize, Debug)]
struct ProposalParams {
    config: planetwars::Config,
    players: Vec<usize>
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct Proposal {
    id: String,
    owner_id: usize,
    config: planetwars::Config,
    players: Vec<ProposalPlayer>,
    #[serde(flatten)]
    status: ProposalStatus,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct ProposalPlayer {
    player_id: usize,
    status: AcceptedState,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
enum AcceptedState {
    Unanswered,
    Accepted,
    Rejected,
}


#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all="camelCase")]
#[serde(tag="status")]
enum ProposalStatus {
    Pending,
    Denied,
    Accepted { match_id: String },
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
#[serde(rename_all="camelCase")]
enum MatchStatus {
    Playing,
    Done,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct MatchMeta {
    id: String,
    timestamp: DateTime<Utc>,
    config: planetwars::Config,
    // TODO: this should maybe be a hashmap for the more general case
    players: Vec<usize>,
    status: MatchStatus,
}

// TODO
#[derive(Serialize, Deserialize)]
#[serde(tag="type", content = "data")]
#[serde(rename_all="camelCase")]
enum LobbyEvent {
    LobbyState(StrippedLobby),
    PlayerData(StrippedPlayer),
    ProposalData(Proposal),
    MatchData(MatchMeta),
}

async fn run_match(
    mut clients: Vec<ClientHandle>,
    serv: GameServer,
    config: planetwars::Config,
    log: MsgStreamHandle<String>)
{
    let event_bus = Arc::new(Mutex::new(EventBus::new()));
    let players = stream::iter(clients.iter_mut().enumerate())
        .then(|(i, client)| {
            let player_token: Token = rand::thread_rng().gen();
            let player_id = (i+1) as u32;
            let player = serv.conn_table().open_connection(player_token, player_id, event_bus.clone());
            client.run_player(player_token).map(move |_| (player_id, Box::new(player) as Box<dyn PlayerHandle>))
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
    player_params: PlayerParams,
) -> Response {
    let mut manager = mgr.lock().unwrap();
    // TODO: translate this in a filter maybe?
    let lobby_id = id.to_lowercase();
    let game_mgr = manager.game_manager.clone();

    // TODO: check for uniqueness of name and token

    let player_data = {
        let lobby = match manager.lobbies.get_mut(&lobby_id) {
            None => return warp::http::StatusCode::NOT_FOUND.into_response(),
            Some(lobby) => lobby,
        };

        let player_id = lobby.token_player.get(&player_params.token).cloned().unwrap_or_else(|| {
            let id = lobby.next_player_id;
            lobby.next_player_id += 1;
            id   
        });
        let player = Player {
            id: player_id,
            name: player_params.name,
            token: player_params.token,
            // TODO?
            connection_count: 0,
            client_connected: game_mgr
                .lock()
                .unwrap()
                .game_server
                .client_manager()
                .is_connected(&player_params.token)
        };
        lobby.token_player.insert(player.token.clone(), player_id);
        lobby.players.insert(player_id, player.clone());
        StrippedPlayer::from(player)
    };

    // register token to player
    manager.token_player
        .entry(player_params.token.clone())
        .or_insert_with(|| HashSet::new())
        .insert((lobby_id.clone(), player_data.id));

    // update other clients
    manager.send_update(&lobby_id, LobbyEvent::PlayerData(player_data.clone()));

    return json(&player_data).into_response();
}

fn update_player_in_lobby(
    id: String,
    _name: String,
    mgr: Arc<Mutex<LobbyManager>>,
    _authorization: Option<String>,
    player_params: PlayerParams,
) -> Response {
    let mut manager = mgr.lock().unwrap();
    let lobby_id = id.to_lowercase();
    let player = {
        let lobby = match manager.lobbies.get_mut(&id.to_lowercase()) {
            None => return warp::http::StatusCode::NOT_FOUND.into_response(),
            Some(lobby) => lobby,
        };
        let player_id = match lobby.token_player.get(&player_params.token) {
            None => return warp::http::StatusCode::NOT_FOUND.into_response(),
            Some(id) => id,
        };
        let player = lobby.players.get_mut(&player_id).unwrap();
        player.name = player_params.name;
        StrippedPlayer::from(player.clone())
    };
    manager.send_update(&lobby_id, LobbyEvent::PlayerData(player.clone()));
    json(&player).into_response()
}

fn remove_player_from_lobby(
    id: String,
    player_id: usize,
    mgr: Arc<Mutex<LobbyManager>>,
    authorization: Option<String>,
) -> Response {
    // TODO: this method is defunct
    let mut manager = mgr.lock().unwrap();
    match manager.lobbies.get_mut(&id.to_lowercase()) {
        Some(lobby) => {
            match lobby.players.get(&player_id) {
                Some(player) => {
                    if player.authorize_header(&authorization) || lobby.authorize_header(&authorization){
                        lobby.players.remove(&player_id);
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

fn parse_auth(hex_token: &str) -> Option<Token> {
    Token::from_hex(hex_token).ok()
}

fn auth_player(auth: &Option<String>, lobby: &Lobby) -> Option<usize> {
    auth.as_ref()
        .and_then(|val| val.strip_prefix("Bearer "))
        .and_then(parse_auth)
        .and_then(|t| lobby.token_player.get(&t).cloned())
}

fn add_proposal_to_lobby(
    lobby_id: String,
    mgr: Arc<Mutex<LobbyManager>>,
    authorization: Option<String>,
    params: ProposalParams
) -> Response {
    let mut manager = mgr.lock().unwrap();
    let lobby_id = lobby_id.to_lowercase();


    let proposal = {
        let lobby = match manager.lobbies.get_mut(&lobby_id) {
            None => return warp::http::StatusCode::NOT_FOUND.into_response(),
            Some(lobby) => lobby,
        };

        let player_id = match auth_player(&authorization, &lobby) {
            Some(id) => id,
            None => return warp::http::StatusCode::UNAUTHORIZED.into_response(),
        };

        let proposal = Proposal {
            owner_id: player_id,
            config: params.config,
            players: params.players.iter().map(|&player_id| {
                ProposalPlayer {
                    player_id,
                    status: AcceptedState::Unanswered
                }
            }).collect(),
            id: Uuid::new_v4().to_hyphenated().to_string(),
            status: ProposalStatus::Pending,
        };

        lobby.proposals.insert(proposal.id.clone(), proposal.clone());
        proposal  
    };

    manager.send_update(&lobby_id, LobbyEvent::ProposalData(proposal.clone()));

    warp::reply::with_status(
        json(&proposal),
        warp::http::StatusCode::OK,
    ).into_response()
}

fn start_proposal(
    lobby_id: String,
    proposal_id: String,
    mgr: Arc<Mutex<LobbyManager>>,
    authorization: Option<String>,
) -> Response {
    let lobby_id = lobby_id.to_lowercase();
    let mut manager = mgr.lock().unwrap();

    let proposal = {
        let game_mgr = manager.game_manager.clone();
        let mut game_manager = game_mgr.lock().unwrap();

        let lobby = match manager.lobbies.get_mut(&lobby_id) {
            None => {
                return warp::reply::with_status(
                    format!("Lobby {} not found", lobby_id),
                    warp::http::StatusCode::NOT_FOUND
                ).into_response()
            }
            Some(lobby) => lobby,
        };
        let player_id = match auth_player(&authorization, &lobby) {
            Some(id) => id,
            None => return warp::http::StatusCode::UNAUTHORIZED.into_response(),
        };
    
        let proposal = match lobby.proposals.get_mut(&proposal_id) {
            None => {
                return warp::reply::with_status(
                    format!("Proposal {} not found", proposal_id),
                    warp::http::StatusCode::NOT_FOUND
                ).into_response()
            }
            Some(proposal) => {
                match proposal.status {
                    ProposalStatus::Pending => proposal,
                    _ => return warp::reply::with_status(
                        format!("Proposal {} is no longer valid", proposal_id),
                        warp::http::StatusCode::BAD_REQUEST,
                    ).into_response()
                }
            }
        };

        if player_id != proposal.owner_id {
            return warp::http::StatusCode::UNAUTHORIZED.into_response();
        }

        let mut tokens = vec![];

        for accepting_player in proposal.players.iter() {
            let player_opt = lobby.players.get(&accepting_player.player_id);
            // I think this should never happen?
            if player_opt.is_none() {
                return warp::reply::with_status(
                    format!("Player {} does not exist in this lobby", accepting_player.player_id),
                    warp::http::StatusCode::BAD_REQUEST
                ).into_response();
            }
            let player = player_opt.unwrap();
            if accepting_player.status != AcceptedState::Accepted || !player.client_connected {
                return warp::reply::with_status(
                    "Not all players are ready",
                    warp::http::StatusCode::BAD_REQUEST
                ).into_response();
            }

            tokens.push(player.token);
        }
        let match_config: planetwars::Config = proposal.config.clone();

        let cb_mgr = mgr.clone();
        let cb_lobby_id = lobby_id.clone();
        let match_id = game_manager.create_match(tokens, match_config.clone(), move |match_id| {
            println!("completed match {}", &match_id);
            let mut mgr = cb_mgr.lock().unwrap();
            mgr.lobbies.get_mut(&cb_lobby_id).and_then(|lobby| {
                if let Some(match_meta) = lobby.matches.get_mut(&match_id) {
                    match_meta.status = MatchStatus::Done;
                    Some(match_meta.clone())
                } else {
                    None
                }
            }).map(|match_meta| {
                mgr.send_update(&cb_lobby_id, LobbyEvent::MatchData(match_meta));
            });
        });
        let match_meta = MatchMeta {
            id: match_id.clone(),
            timestamp: Utc::now(),
            status: MatchStatus::Playing,
            config: proposal.config.clone(),
            players: proposal.players.iter().map(|p| p.player_id.clone()).collect(),
        };
        lobby.matches.insert(match_meta.id.clone(), match_meta.clone());
        proposal.status = ProposalStatus::Accepted { match_id };
        proposal.clone()
    };


    manager.send_update(&lobby_id, LobbyEvent::ProposalData(proposal.clone()));

    return warp::reply::with_status(
        json(&proposal),
        warp::http::StatusCode::OK
    ).into_response();
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct AcceptParams {
    status: AcceptedState,
}

fn set_player_accepted_state(
    lobby_id: String,
    proposal_id: String,
    mgr: Arc<Mutex<LobbyManager>>,
    authorization: Option<String>,
    params: AcceptParams,
) -> Response {
    let mut manager = mgr.lock().unwrap();
    let lobby_id = lobby_id.to_lowercase();

    let proposal = {
        let lobby = match manager.lobbies.get_mut(&lobby_id) {
            None => {
                return warp::reply::with_status(
                    format!("Lobby {} not found", lobby_id),
                    warp::http::StatusCode::NOT_FOUND
                ).into_response()
            }
            Some(lobby) => lobby,
        };

        let player_id = match auth_player(&authorization, &lobby) {
            Some(id) => id,
            None => return warp::http::StatusCode::UNAUTHORIZED.into_response(),
        };        

        let proposal = match lobby.proposals.get_mut(&proposal_id) {
            Some(proposal) => proposal,
            None => return warp::reply::with_status(
                format!("Proposal {} not found", proposal_id),
                warp::http::StatusCode::NOT_FOUND
            ).into_response()
        };
        
        for player in proposal.players.iter_mut() {
            if player.player_id == player_id {
                player.status = params.status.clone();
            }
        }

        match params.status {
            AcceptedState::Rejected => proposal.status = ProposalStatus::Denied,
            _ => (),
        };

        proposal.clone()
    };

    manager.send_update(&lobby_id, LobbyEvent::ProposalData(proposal.clone()));

    warp::reply::with_status(
        json(&proposal),
        warp::http::StatusCode::OK
    ).into_response()
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
    #[serde(with = "hex")]
    token: Token,
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


struct LobbyRequestCtx {
    lobby_mgr: Arc<Mutex<LobbyManager>>,
    lobby_id: String,
    auth_header: Option<String>,
}

fn lobby_context(lobby_mgr: Arc<Mutex<LobbyManager>>)
    -> impl Filter<Extract=(LobbyRequestCtx, ), Error=Rejection> + Clone
{
    warp::path("lobbies")
        .and(with_lobby_manager(lobby_mgr))
        .and(warp::path::param::<String>())
        .and(warp::header::optional("authorization"))
        .map(|lobby_mgr, lobby_id, auth_header|
            LobbyRequestCtx {
                lobby_mgr,
                lobby_id,
                auth_header: auth_header,
            }
        )
}

impl LobbyRequestCtx {
    fn get_lobby(self) -> Response {
        let mgr = self.lobby_mgr.lock().unwrap();
        match mgr.lobbies.get(&self.lobby_id.to_lowercase()) {
            Some(lobby) => {
                json(&StrippedLobby::from(lobby.clone())).into_response()
            },
            None => warp::http::StatusCode::NOT_FOUND.into_response()
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
    init_callbacks(lobby_manager.clone());

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

    let lobby_scope = warp::path("lobbies")
        .and(warp::filters::path::param::<String>());

    // GET /lobbies/<id>
    let get_lobbies_id_route =
        lobby_context(lobby_manager.clone())
        .and(warp::path::end())
        .and(warp::get())
        .map(LobbyRequestCtx::get_lobby);

    // PUT /lobbies/<id>
    let put_lobbies_id_route = 
        lobby_scope
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
    let delete_lobbies_id_players_route = warp::path!("lobbies" / String / "players" / usize)
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
