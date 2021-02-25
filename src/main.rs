#![feature(async_closure)]

use chrono::{DateTime, Utc};
use mozaic_core::{EventBus, match_context::PlayerHandle, msg_stream::msg_stream};
use planetwars::MatchConfig;
use serde::{Deserialize, Serialize};

use mozaic_core::client_manager::ClientHandle;
use futures::{FutureExt, StreamExt, stream};

mod planetwars;
mod pw_maps;

use mozaic_core::{Token, GameServer, MatchCtx};
use mozaic_core::msg_stream::{MsgStreamHandle};
use tokio::sync::mpsc;
use uuid::Uuid;

use warp::{Rejection, reply::{Reply, Response, json}};
use warp::Filter;


use std::{convert::Infallible, path::Path, sync::{Arc, Mutex}};
use std::collections::{HashMap, HashSet};

mod websocket;

use hex::FromHex;
use rand::Rng;

const MAPS_DIRECTORY: &'static str = "maps";

#[derive(Debug, Clone)]
struct Player {
    /// Scoped in lobby
    id: usize,
    name: String,
    token: Token,
    connection_count: usize,
    client_connected: bool,
}

#[derive(Serialize, Deserialize, Debug)]
struct PlayerParams {
    name: String,
    #[serde(with = "hex")]
    token: Token,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct StrippedPlayer {
    id: usize,
    name: String,
    connected: bool,
    client_connected: bool,
}

pub struct GameManager {
    game_server: GameServer,
    matches: HashMap<String, MatchData>
}

pub struct MatchData {
    log: MsgStreamHandle<String>,
}

impl GameManager {
    fn create_match<F>(&mut self, tokens: Vec<Token>, match_config: MatchConfig, cb: F) -> String
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
            match_config,
            log).map(|_| cb(cb_match_id))
        );
        return match_id;
    }

    fn list_matches(&self) -> Vec<String> {
        self.matches.keys().cloned().collect()
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all="camelCase")]
pub struct PwMapData {
    pub name: String,
    pub max_players: usize,
}

pub type PwMaps = HashMap<String, PwMapData>;

#[derive(Clone, Debug)]
pub struct Lobby {
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
pub struct StrippedLobby {
    id: String,
    name: String,
    public: bool,
    players: HashMap<usize,StrippedPlayer>,
    proposals: HashMap<String, Proposal>,
    matches: HashMap<String, MatchMeta>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct LobbyConfig {
    pub name: String,
    pub public: bool,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct MatchStartConfig {
    pub players: Vec<String>
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

pub struct LobbyManager {
    game_manager: Arc<Mutex<GameManager>>,
    lobbies: HashMap<String, Lobby>,
    maps: Arc<PwMaps>,
    websocket_connections: HashMap<String, Vec<WsConnection>>,
    /// token -> {(lobby_id, player_id)}
    token_player: HashMap<Token, HashSet<(String, usize)>>,
}

impl LobbyManager {
    pub fn new(game_manager: Arc<Mutex<GameManager>>, maps: PwMaps) -> Self {
        Self {
            game_manager,
            lobbies: HashMap::new(),
            websocket_connections: HashMap::new(),
            token_player: HashMap::new(),
            maps: Arc::new(maps),
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
#[serde(rename_all="camelCase")]
struct ProposalParams {
    config: planetwars::MatchConfig,
    players: Vec<usize>
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Proposal {
    id: String,
    owner_id: usize,
    config: planetwars::MatchConfig,
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


#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
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
pub struct MatchMeta {
    id: String,
    timestamp: DateTime<Utc>,
    config: planetwars::MatchConfig,
    // TODO: this should maybe be a hashmap for the more general case
    players: Vec<usize>,
    status: MatchStatus,
}

// TODO
#[derive(Serialize, Deserialize)]
#[serde(tag="type", content = "data")]
#[serde(rename_all="camelCase")]
pub enum LobbyEvent {
    LobbyState(StrippedLobby),
    PlayerData(StrippedPlayer),
    ProposalData(Proposal),
    MatchData(MatchMeta),
}

async fn run_match(
    mut clients: Vec<ClientHandle>,
    serv: GameServer,
    config: planetwars::MatchConfig,
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



#[derive(Serialize, Deserialize, Debug, Clone)]
struct AcceptParams {
    status: AcceptedState,
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

enum LobbyApiError {
    LobbyNotFound,
    NotAuthenticated,
    NotAuthorized,
    InvalidProposalParams(String),
    ProposalNotFound,
    ProposalExpired,
    ProposalNotReady,
}

type LobbyApiResult<T> = Result<T, LobbyApiError>;

fn result_to_response<T>(result: LobbyApiResult<T>) -> Response
    where T: warp::Reply
{
    match result {
        Ok(reply) => reply.into_response(),
        Err(LobbyApiError::LobbyNotFound) => warp::http::StatusCode::NOT_FOUND.into_response(),
        Err(LobbyApiError::NotAuthenticated) => warp::http::StatusCode::BAD_REQUEST.into_response(),
        Err(LobbyApiError::NotAuthorized) => warp::http::StatusCode::UNAUTHORIZED.into_response(),
        Err(LobbyApiError::InvalidProposalParams(msg)) => warp::reply::with_status(
            msg,
            warp::http::StatusCode::BAD_REQUEST
        ).into_response(),
        Err(LobbyApiError::ProposalNotFound) => warp::http::StatusCode::NOT_FOUND.into_response(),
        Err(LobbyApiError::ProposalExpired) => warp::reply::with_status(
            "proposal is no longer valid",
            warp::http::StatusCode::BAD_REQUEST,
        ).into_response(),
        Err(LobbyApiError::ProposalNotReady) => warp::reply::with_status(
            "not all players have accepted",
            warp::http::StatusCode::BAD_REQUEST,
        ).into_response(),
    }
}

fn json_response<T>(result: LobbyApiResult<T>) -> Response
    where T: Serialize
{
    let res = result.map(|value| json(&value));
    return result_to_response(res);
}

struct LobbyRequestCtx {
    lobby_mgr: Arc<Mutex<LobbyManager>>,
    lobby_id: String,
    auth_header: Option<String>,
}

impl LobbyRequestCtx {
    fn with_lobby<F, T>(&self, fun: F) -> Result<T, LobbyApiError>
        where F: FnOnce(&mut Lobby) -> Result<T, LobbyApiError>
    {
        let lobby_id = self.lobby_id.to_lowercase();
        let mut mgr = self.lobby_mgr.lock().unwrap();
        let res = mgr.lobbies.get_mut(&lobby_id)
            .ok_or(LobbyApiError::LobbyNotFound)
            .and_then(fun);
        return res;
    }

    fn broadcast_event(&self, ev: LobbyEvent) {
        let lobby_id = self.lobby_id.to_lowercase();
        let mut mgr = self.lobby_mgr.lock().unwrap();
        mgr.send_update(&lobby_id, ev);
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



fn lobby_context<MountPoint>(
    base: MountPoint,
    lobby_mgr: Arc<Mutex<LobbyManager>>)
    -> impl Filter<Extract=(LobbyRequestCtx, ), Error=Rejection> + Clone
    where MountPoint: Filter<Extract=(String, ), Error=Rejection> + Clone
{
    base
        .and(with_lobby_manager(lobby_mgr))
        .and(warp::header::optional("authorization"))
        .map(|lobby_id, lobby_mgr, auth_header|
            LobbyRequestCtx {
                lobby_mgr,
                lobby_id,
                auth_header,
            }
        )
}

fn get_lobby_by_id(req: LobbyRequestCtx) -> Response {
    let res = req.with_lobby(|lobby| {
        Ok(json(&StrippedLobby::from(lobby.clone())))
    });
    return result_to_response(res)
}



fn join_lobby(req: LobbyRequestCtx, player_params: PlayerParams)
    -> Response
{
    let player_token = player_params.token;
    // TODO: check for uniqueness of name and token
    let game_manager = req.lobby_mgr.lock().unwrap().game_manager.clone();
    let res = req.with_lobby(|lobby| {
        let player_id = lobby.token_player.get(&player_params.token).cloned().unwrap_or_else(|| {
            let id = lobby.next_player_id;
            lobby.next_player_id += 1;
            id   
        });
        let player = Player {
            id: player_id,
            name: player_params.name,
            token: player_params.token.clone(),
            // TODO?
            connection_count: 0,
            client_connected: game_manager
                .lock()
                .unwrap()
                .game_server
                .client_manager()
                .is_connected(&player_params.token)
        };
        lobby.token_player.insert(player.token.clone(), player_id);
        lobby.players.insert(player_id, player.clone());
        Ok(StrippedPlayer::from(player))
    });
    if let Ok(player_data) = &res {
        req.broadcast_event(LobbyEvent::PlayerData(player_data.clone()));
        req.lobby_mgr
            .lock()
            .unwrap()
            .token_player
            .entry(player_token)
            .or_insert_with(|| HashSet::new())
            .insert((req.lobby_id.to_lowercase(), player_data.id));
    }

    return json_response(res);
}

const MAX_TURNS_ALLOWED: usize = 500;

fn create_proposal(req: LobbyRequestCtx, params: ProposalParams)
    -> Response
{
    let auth = &req.auth_header;
    let maps = req.lobby_mgr.lock().unwrap().maps.clone();
    let res = req.with_lobby(|lobby| {
        let player_id = auth_player(auth, lobby)
            .ok_or(LobbyApiError::NotAuthenticated)?;

        if params.config.max_turns > MAX_TURNS_ALLOWED {
            return Err(LobbyApiError::InvalidProposalParams(
                format!("max allowed turns is {}", MAX_TURNS_ALLOWED))
            );
        }

        let map_data = maps.get(&params.config.map_name)
            .ok_or_else(|| LobbyApiError::InvalidProposalParams(
                "map does not exist".to_string()
            ))?;
        
        if params.players.len() > map_data.max_players {
            return Err(LobbyApiError::InvalidProposalParams(
                format!("too many players")
            ));
        }

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

        Ok(proposal)
    });

    if let Ok(proposal) = &res {
        req.broadcast_event(LobbyEvent::ProposalData(proposal.clone()));
    }

    return json_response(res);
}

fn start_proposal(req: LobbyRequestCtx, proposal_id: String) -> Response {
    let manager = req.lobby_mgr.clone();
    let game_manager = req.lobby_mgr.lock().unwrap().game_manager.clone();

    let auth = &req.auth_header;
    let res = req.with_lobby(|lobby| {
        let player_id = auth_player(auth, lobby)
            .ok_or(LobbyApiError::NotAuthenticated)?;
        let proposal = lobby.proposals.get_mut(&proposal_id)
            .ok_or(LobbyApiError::ProposalNotFound)?;

        if player_id != proposal.owner_id {
            return Err(LobbyApiError::NotAuthorized);
        }

        match proposal.status {
            ProposalStatus::Pending => (),
            _ => return Err(LobbyApiError::ProposalExpired)
        }

        let mut tokens = vec![];

        for accepting_player in proposal.players.iter() {
            // player should exist. TODO: maybe make this more safe?
            let player = lobby.players.get(&accepting_player.player_id).unwrap();

            if accepting_player.status != AcceptedState::Accepted || !player.client_connected {
                return Err(LobbyApiError::ProposalNotReady);
            }

            tokens.push(player.token);
        }
        let match_config = proposal.config.clone();

        let cb_mgr = manager.clone();
        let cb_lobby_id = lobby.id.clone();
        let match_id = game_manager.lock().unwrap().create_match(tokens, match_config.clone(), move |match_id| {
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
        Ok(proposal.clone())
    });

    if let Ok(proposal) = &res {
        req.broadcast_event(LobbyEvent::ProposalData(proposal.clone()));
    }

    return json_response(res);
}

fn accept_proposal(
    req: LobbyRequestCtx,
    proposal_id: String,
    params: AcceptParams,
) -> Response {
    let res = req.with_lobby(|lobby| {
        let player_id = auth_player(&req.auth_header, lobby)
            .ok_or(LobbyApiError::NotAuthenticated)?;

        let proposal = lobby.proposals.get_mut(&proposal_id)
            .ok_or(LobbyApiError::ProposalNotFound)?;
        
        for player in proposal.players.iter_mut() {
            if player.player_id == player_id {
                player.status = params.status.clone();
            }
        }

        match params.status {
            AcceptedState::Rejected => proposal.status = ProposalStatus::Denied,
            _ => (),
        };

        Ok(proposal.clone())
    });

    if let Ok(proposal) = &res {
        req.broadcast_event(LobbyEvent::ProposalData(proposal.clone()));
    }

    return json_response(res);
}

fn get_maps(mgr: Arc<Mutex<LobbyManager>>) -> impl Reply {
    let mgr = mgr.lock().unwrap();
    let maps: Vec<&PwMapData> = mgr.maps.values().collect();
    return json(&maps);
}

#[tokio::main]
async fn main() {
    let maps = pw_maps::build_map_index(Path::new(MAPS_DIRECTORY))
        .expect("failed to read maps");
    let game_server = GameServer::new();

    // TODO: can we run these on the same port? Would that be desirable?
    tokio::spawn(game_server.run_ws_server("127.0.0.1:8080".to_string()));

    let game_manager = Arc::new(Mutex::new(GameManager {
        game_server,
        matches: HashMap::new(),
    }));

    let lobby_manager = Arc::new(Mutex::new(LobbyManager::new(game_manager.clone(), maps)));
    init_callbacks(lobby_manager.clone());

    // POST /lobbies
    let post_lobbies_route = warp::path!("lobbies")
        .and(warp::post())
        .and(with_lobby_manager(lobby_manager.clone()))
        .and(warp::body::json())
        .map(create_lobby);

    let m = lobby_manager.clone();
    let lobby_scope =  move || lobby_context(
        warp::path!("lobbies" / String / .. ),
        m.clone(),
    );

    // GET /lobbies/<id>
    let get_lobbies_id_route =
        lobby_scope()
            .and(warp::path::end())
            .and(warp::get())
            .map(get_lobby_by_id);

    // POST /lobbies/<id>/join
    let post_lobbies_id_players_route =
        lobby_scope()
            .and(warp::path!("join"))
            .and(warp::post())
            .and(warp::body::json())
            .map(join_lobby);

    // POST /lobbies/<id>/proposals
    let post_lobbies_id_proposals_route = 
        lobby_scope()
            .and(warp::path!("proposals"))
            .and(warp::post())
            .and(warp::body::json())
            .map(create_proposal);
    
    // POST /lobbies/<lobby_id>/proposals/<proposal_id>/start
    let post_lobbies_id_proposals_id_start_route = 
        lobby_scope()
            .and(warp::path!("proposals" / String / "start"))
            .and(warp::post())
            .map(start_proposal);
    
    // POST /lobbies/<lobby_id>/proposals/<proposal_id>/accept
    let post_lobbies_id_proposals_id_accept_route =
        lobby_scope()
            .and(warp::path!("proposals" / String / "accept"))
            .and(warp::post())
            .and(warp::body::json())
            .map(accept_proposal);
    
    let get_maps_route = warp::path!("maps")
        .and(warp::get())
        .and(with_lobby_manager(lobby_manager.clone()))
        .map(get_maps);
    
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
            ws.on_upgrade(move |socket| websocket::handle_websocket(socket, mgr))
        });

    let routes = post_lobbies_route
                              .or(get_lobbies_id_route)
                              .or(post_lobbies_id_players_route)
                              .or(post_lobbies_id_proposals_id_start_route)
                              .or(post_lobbies_id_proposals_route)
                              .or(post_lobbies_id_proposals_id_accept_route)
                              .or(get_maps_route)
                              .or(get_matches_route)
                              .or(get_match_route)
                              .or(websocket_route);
    
    warp::serve(routes).run(([127, 0, 0, 1], 7412)).await;
}
