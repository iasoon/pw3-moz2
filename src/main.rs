#![feature(async_closure)]

use mozaic_core::client_manager::ClientHandle;
use futures::future;

mod planetwars;

use mozaic_core::{Token, GameServer, MatchCtx};

use std::convert::Infallible;
use warp::reply::{json,Reply,Response};
use warp::Filter;
use serde::{Serialize,Deserialize};

use std::sync::{Arc, Mutex};
use std::collections::HashMap;

use hex::FromHex;
use rand::Rng;

#[derive(Serialize, Deserialize, Debug)]
struct MatchConfig {
    client_tokens: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct Player {
    name: String,
    #[serde(with = "hex")]
    token: Token,
    ready: bool,
}

#[derive(Serialize, Deserialize, Debug)]
struct StrippedPlayer {
    name: String,
    ready: bool,
}

struct GameManager {
    game_server: GameServer,
}

impl GameManager {
    fn create_match(&mut self, config: MatchConfig) {
        let clients = config.client_tokens.iter().map(|token_hex| {
            let token = Token::from_hex(&token_hex).unwrap();
            self.game_server.get_client(&token)
        }).collect::<Vec<_>>();
    
        let match_ctx = self.game_server.create_match();
        tokio::spawn(run_match(clients, match_ctx));
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct Lobby {
    id: String,
    name: String,
    public: bool,
    match_config: planetwars::Config,
    players: Vec<Player>,
    #[serde(with = "hex")]
    lobby_token: Token
}

#[derive(Serialize, Deserialize, Debug)]
struct StrippedLobby {
    id: String,
    name: String,
    public: bool,
    match_config: planetwars::Config,
    players: Vec<Player>,
}

#[derive(Serialize, Deserialize, Debug)]
struct LobbyConfig {
    name: String,
    public: bool,
    match_config: planetwars::Config,
}

impl From<LobbyConfig> for Lobby {
    fn from(config: LobbyConfig) -> Lobby {
        let id: [u8; 16] = rand::thread_rng().gen();
        Lobby {
            id: hex::encode(id),
            name: config.name,
            public: config.public,
            match_config: config.match_config,
            players: vec![],
            lobby_token: rand::thread_rng().gen(),
        }
    }
}

impl From<Lobby> for StrippedLobby {
    fn from(lobby: Lobby) -> StrippedLobby {
        StrippedLobby {
            id: lobby.id,
            name: lobby.name,
            public: lobby.public,
            match_config: lobby.match_config,
            players: lobby.players,
        }
    }
}

struct LobbyManager {
    game_manager: Arc<Mutex<GameManager>>,
    lobbies: HashMap<String, Lobby>
}

impl LobbyManager {
    pub fn new(game_manager: Arc<Mutex<GameManager>>) -> Self {
        Self {
            game_manager,
            lobbies: HashMap::new(),
        }
    }

    pub fn create_lobby(&mut self, config: LobbyConfig) -> Lobby {
        let lobby: Lobby = config.into();
        self.lobbies.insert(lobby.id.clone(), lobby.clone());
        lobby
    }
}

async fn run_match(mut clients: Vec<ClientHandle>, mut match_ctx: MatchCtx) {
    let players = clients.iter_mut().enumerate().map(|(i, client)| {
        let player_token: Token = rand::thread_rng().gen();
        match_ctx.create_player(i as u32, player_token);
        client.run_player(player_token)
    }).collect::<Vec<_>>();

    let config = planetwars::Config {
        map_file: "hex.json".to_string(),
        max_turns: 500,
    };

    future::join_all(players).await;
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

fn create_match(
    mgr: Arc<Mutex<GameManager>>,
    match_config: MatchConfig,
) -> impl Reply {
    let mut manager = mgr.lock().unwrap();
    manager.create_match(match_config);
    return "sure bro";
}

fn create_lobby(
    mgr: Arc<Mutex<LobbyManager>>,
    lobby_config: LobbyConfig,
) -> impl Reply {
    let mut manager = mgr.lock().unwrap();
    let lobby = manager.create_lobby(lobby_config);
    json(&lobby)
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
    authorization: Option<String>,
) -> Response {
    let manager = mgr.lock().unwrap();
    match manager.lobbies.get(&id.to_lowercase()) {
        Some(lobby) => {
            if authorization.is_none() {
                json(&StrippedLobby::from(lobby.clone())).into_response()
            } else {
                let bearer_token = authorization.unwrap().to_lowercase();
                let token_string = bearer_token.strip_prefix("bearer ");
                if token_string.is_none() {
                    return json(&StrippedLobby::from(lobby.clone())).into_response();
                }
                let token_opt = Token::from_hex(token_string.unwrap());
                if token_opt.is_err() || token_opt.unwrap() != lobby.lobby_token {
                    json(&StrippedLobby::from(lobby.clone())).into_response()
                } else {
                    json(&lobby).into_response()
                }
            }
        },
        None => warp::http::StatusCode::NOT_FOUND.into_response()
    }
}

#[tokio::main]
async fn main() {
    let game_server = GameServer::new();
    // TODO: can we run these on the same port? Would that be desirable?
    tokio::spawn(game_server.run_ws_server("127.0.0.1:8080".to_string()));

    let game_manager = Arc::new(Mutex::new(GameManager { game_server }));
    let lobby_manager = Arc::new(Mutex::new(LobbyManager::new(game_manager.clone())));

    let matches_route = warp::path("matches")
        .and(warp::post())
        .and(with_game_manager(game_manager))
        .and(warp::body::json())
        .map(create_match);

    let post_lobby_route = warp::path("lobbies")
        .and(warp::path::end())
        .and(warp::post())
        .and(with_lobby_manager(lobby_manager.clone()))
        .and(warp::body::json())
        .map(create_lobby);

    let get_lobby_route = warp::path("lobbies")
        .and(warp::path::end())
        .and(warp::get())
        .and(with_lobby_manager(lobby_manager.clone()))
        .map(get_lobbies);

    let get_lobby_id_route = warp::path("lobbies")
        .and(warp::path::param())
        .and(warp::path::end())
        .and(warp::get())
        .and(with_lobby_manager(lobby_manager.clone()))
        .and(warp::header::optional::<String>("authorization"))
        .map(get_lobby_by_id);

    let routes = matches_route.or(post_lobby_route)
                              .or(get_lobby_id_route)
                              .or(get_lobby_route);

    warp::serve(routes).run(([127, 0, 0, 1], 3000)).await;
}
