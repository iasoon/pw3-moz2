#![feature(async_closure)]

use chrono::{Utc};
use lobby_manager::*;
use serde::{Deserialize, Serialize};

mod planetwars;
mod pw_maps;
mod websocket;
mod game_manager;
mod lobby_manager;

use mozaic_core::{Token};
use uuid::Uuid;

use warp::{Rejection, reply::{Reply, Response, json}};
use warp::Filter;


use std::{convert::Infallible, path::Path, sync::{Arc, Mutex}};
use std::collections::HashSet;

use hex::FromHex;

use game_manager::{GameManager, MatchData, MatchPlayer, create_match, read_match_log_from_disk};

const MAPS_DIRECTORY: &'static str = "maps";


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

#[derive(Serialize, Deserialize, Debug)]
pub struct LobbyConfig {
    pub name: String,
    pub public: bool,
}

fn create_lobby(
    mgr: Arc<Mutex<LobbyManager>>,
    lobby_config: LobbyConfig,
) -> impl Reply {
    let mut manager = mgr.lock().unwrap();
    let lobby = manager.create_lobby(lobby_config);
    json(&LobbyData::from(lobby.clone()))
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
    match manager.get_match_data(&match_id) {
        Some(MatchData::Finished { log_path }) => {
            match read_match_log_from_disk(log_path) {
                Ok(log) => json(&log).into_response(),
                Err(err) => warp::reply::with_status(
                    err.to_string(),
                    warp::http::StatusCode::INTERNAL_SERVER_ERROR,
                ).into_response()
            }
        }
        _ => warp::http::StatusCode::NOT_FOUND.into_response(),
    }
}


enum LobbyApiError {
    LobbyNotFound,
    NotAuthenticated,
    NotAuthorized,
    NameTaken,
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
        Err(LobbyApiError::NameTaken) => warp::reply::with_status(
            "name is not available",
            warp::http::StatusCode::BAD_REQUEST,
        ).into_response(),
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
        Ok(json(&LobbyData::from(lobby.clone())))
    });
    return result_to_response(res)
}

#[derive(Serialize, Deserialize, Debug)]
struct PlayerParams {
    name: String,
    #[serde(with = "hex")]
    token: Token,
}

fn join_lobby(req: LobbyRequestCtx, player_params: PlayerParams)
    -> Response
{
    let player_token = player_params.token;
    // TODO: check for uniqueness of name and token
    let game_manager = req.lobby_mgr.lock().unwrap().game_manager.clone();
    let res = req.with_lobby(|lobby| {
        // whether the player sending the request already has this name
        let owns_name = lobby.token_player
            .get(&player_params.token)
            .map(|player_id| lobby.players[player_id].name == player_params.name)
            .unwrap_or(false);

        // is the requested name available?
        let taken_names: HashSet<&str> = lobby.players.values().map(|player| player.name.as_str()).collect();
        if taken_names.contains(player_params.name.as_str()) && !owns_name {
            return Err(LobbyApiError::NameTaken);
        }

        let player_id = lobby.token_player.get(&player_params.token).cloned().unwrap_or_else(|| {
            let id = lobby.next_player_id;
            lobby.next_player_id += 1;
            id   
        });
        let player = Player {
            id: player_id,
            name: player_params.name,
            player_type: PlayerType::External { token: player_params.token.clone() },
            // TODO: this is where the connection count underflow bug comes from, I think
            connection_count: 0,
            client_connected: game_manager
                .lock()
                .unwrap()
                .client_connected(&player_params.token),
        };
        lobby.token_player.insert(player_params.token.clone(), player_id);
        lobby.players.insert(player_id, player.clone());
        Ok(PlayerData::from(player))
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


#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all="camelCase")]
struct ProposalParams {
    config: planetwars::MatchConfig,
    players: Vec<usize>
}

fn validate_proposal_params(maps: &PwMaps, params: &ProposalParams) -> LobbyApiResult<()> {
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

    Ok(())
}

fn create_proposal(req: LobbyRequestCtx, params: ProposalParams)
    -> Response
{
    let auth = &req.auth_header;
    let maps = req.lobby_mgr.lock().unwrap().maps.clone();
    let res = req.with_lobby(|lobby| {
        validate_proposal_params(maps.as_ref(), &params)?;

        let creator_id = auth_player(auth, lobby)
            .ok_or(LobbyApiError::NotAuthenticated)?;

        let proposal = Proposal {
            owner_id: creator_id,
            config: params.config,
            players: params.players.iter().map(|&player_id| {
                let mut status = AcceptedState::Unanswered;
                if player_id == creator_id {
                    status = AcceptedState::Accepted;
                }

                let player = lobby.players.get(&player_id)
                    .expect("player does not exist");

                if let PlayerType::Internal { .. } = player.player_type {
                    status = AcceptedState::Accepted;
                }

                ProposalPlayer {
                    player_id,
                    status,
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

        let mut players = vec![];

        for accepting_player in proposal.players.iter() {
            // player should exist. TODO: maybe make this more safe?
            let player = lobby.players.get(&accepting_player.player_id).unwrap();

            if accepting_player.status != AcceptedState::Accepted || !player.client_connected {
                return Err(LobbyApiError::ProposalNotReady);
            }

            let match_player = match &player.player_type {
                PlayerType::External { token } => {
                    MatchPlayer::External(token.clone())
                }
                PlayerType::Internal { bot } => {
                    MatchPlayer::Internal(bot.clone())
                }
            };

            players.push(match_player);

        }

        let match_config = proposal.config.clone();

        let cb_mgr = manager.clone();
        let cb_lobby_id = lobby.id.clone();
        let match_id = create_match(game_manager, players, match_config.clone(), move |match_id| {
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

        // TODO: below code is a bit backwards

        // take ownership of the data
        let proposal = proposal.clone();
        // the proposal is no longer open; remove it
        lobby.proposals.remove(&proposal.id);

        Ok((proposal, match_meta))
    });

    if let Ok((proposal, match_meta)) = &res {
        // TODO: spurious clones
        req.broadcast_event(LobbyEvent::ProposalData(proposal.clone()));
        req.broadcast_event(LobbyEvent::MatchData(match_meta.clone()));
    }

    return json_response(res.map(|(proposal, _match)| proposal));
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct AcceptParams {
    status: AcceptedState,
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

        if proposal.players.iter().any(|p| p.status == AcceptedState::Rejected) {
            proposal.status = ProposalStatus::Denied;
        }

        // take ownership of proposal data
        let proposal = proposal.clone();

        // this proposal is no longer relevant; discard it.
        if proposal.status == ProposalStatus::Denied {
            lobby.proposals.remove(&proposal.id);
        }

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
    let game_manager = GameManager::init("0.0.0.0:8080".to_string());
    let lobby_manager = LobbyManager::create(game_manager.clone(), maps);

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
    
    warp::serve(routes).run(([0, 0, 0, 0], 7412)).await;
}
