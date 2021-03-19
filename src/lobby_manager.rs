use std::{collections::{HashMap, HashSet}, sync::{Arc, Mutex}};

use chrono::{DateTime, Utc};
use mozaic_core::Token;
use rand::Rng;
use serde::{Serialize, Deserialize};

use crate::{LobbyConfig, game_manager::GameManager, planetwars, websocket::WsConnection};

pub struct LobbyManager {
    pub game_manager: Arc<Mutex<GameManager>>,
    pub lobbies: HashMap<String, Lobby>,
    pub maps: Arc<PwMaps>,
    pub websocket_connections: HashMap<String, Vec<WsConnection>>,
    /// token -> {(lobby_id, player_id)}
    pub token_player: HashMap<Token, HashSet<(String, usize)>>,
}

impl LobbyManager {
    pub fn create(game_manager: Arc<Mutex<GameManager>>, maps: PwMaps) -> Arc<Mutex<Self>> {
        let mgr = Self {
            game_manager,
            lobbies: HashMap::new(),
            websocket_connections: HashMap::new(),
            token_player: HashMap::new(),
            maps: Arc::new(maps),
        };
        let arc = Arc::new(Mutex::new(mgr));
        init_callbacks(arc.clone());
        return arc;
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
                PlayerData::from(player.clone())
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
    let client_mgr = game_mgr.client_manager_mut();
    
    let cloned_ref = mgr_ref.clone();
    client_mgr.on_connect(Box::new(move |token|
        set_client_connected(&cloned_ref, token, true)));
    let cloned_ref = mgr_ref.clone();
    client_mgr.on_disconnect(Box::new(move |token|
        set_client_connected(&cloned_ref, token, false)));
}

pub type PwMaps = HashMap<String, PwMapData>;

#[derive(Clone, Debug)]
pub struct Lobby {
    pub id: String,
    pub name: String,
    pub public: bool,

    pub next_player_id: usize,
    pub token_player: HashMap<Token, usize>,
    pub players: HashMap<usize, Player>,

    pub proposals: HashMap<String, Proposal>,
    pub matches: HashMap<String, MatchMeta>,
    // #[serde(with = "hex")]
    pub lobby_token: Token,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all="camelCase")]
pub struct PwMapData {
    pub name: String,
    pub max_players: usize,
}


#[derive(Debug, Clone)]
pub struct Player {
    /// Scoped in lobby
    pub id: usize,
    pub name: String,
    pub token: Token,
    pub connection_count: usize,
    pub client_connected: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Proposal {
    pub id: String,
    pub owner_id: usize,
    pub config: planetwars::MatchConfig,
    pub players: Vec<ProposalPlayer>,
    #[serde(flatten)]
    pub status: ProposalStatus,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ProposalPlayer {
    pub player_id: usize,
    pub status: AcceptedState,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum AcceptedState {
    Unanswered,
    Accepted,
    Rejected,
}


#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all="camelCase")]
#[serde(tag="status")]
pub enum ProposalStatus {
    Pending,
    Denied,
    Accepted { match_id: String },
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
#[serde(rename_all="camelCase")]
pub enum MatchStatus {
    Playing,
    Done,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct MatchMeta {
    pub id: String,
    pub timestamp: DateTime<Utc>,
    pub config: planetwars::MatchConfig,
    // TODO: maybe this should be a hashmap for the more general case
    pub players: Vec<usize>,
    pub status: MatchStatus,
}



#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PlayerData {
    pub id: usize,
    pub name: String,
    pub connected: bool,
    pub client_connected: bool,
}



#[derive(Serialize, Deserialize, Debug)]
pub struct LobbyData {
    pub id: String,
    pub name: String,
    pub public: bool,
    pub players: HashMap<usize,PlayerData>,
    pub proposals: HashMap<String, Proposal>,
    pub matches: HashMap<String, MatchMeta>,
}


// TODO: don't use from for nondeterministic conversions
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

impl From<Lobby> for LobbyData {
    fn from(lobby: Lobby) -> LobbyData {
        LobbyData {
            id: lobby.id,
            name: lobby.name,
            public: lobby.public,
            players: lobby.players.iter().map(|(k,v)| (k.clone(),PlayerData::from(v.clone()))).collect(),
            proposals: lobby.proposals.clone(),
            matches: lobby.matches.clone(),
        }
    }
}

impl From<Player> for PlayerData {
    fn from(player: Player) -> PlayerData {
        PlayerData {
            id: player.id,
            name: player.name,
            connected: player.connection_count > 0,
            client_connected: player.client_connected,
        }
    }
}

// TODO
#[derive(Serialize, Deserialize)]
#[serde(tag="type", content = "data")]
#[serde(rename_all="camelCase")]
pub enum LobbyEvent {
    LobbyState(LobbyData),
    PlayerData(PlayerData),
    ProposalData(Proposal),
    MatchData(MatchMeta),
    // Ugly, quickly, dirty, but effective
    MatchLogEvent(MatchLogEvent),
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all="camelCase")]
pub struct MatchLogEvent {
    pub stream_id: usize,
    pub event: String,
}