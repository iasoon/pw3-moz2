#![feature(async_closure)]

use serde::{Deserialize, Serialize};

use mozaic_core::client_manager::ClientHandle;
use futures::future;

mod planetwars;

use mozaic_core::{Token, GameServer, MatchCtx};
use mozaic_core::msg_stream::{MsgStreamHandle};
use std::collections::HashMap;

use std::convert::Infallible;
use warp::reply::{json, Json, Reply};
use warp::Filter;

use std::sync::{Arc, Mutex};

use hex::FromHex;
use rand::Rng;

#[derive(Serialize, Deserialize, Debug)]
struct MatchConfig {
    client_tokens: Vec<String>,
}

struct GameManager {
    game_server: GameServer,
    matches: HashMap<String, MatchData>
}

struct MatchData {
    log: MsgStreamHandle<String>,
}

impl GameManager {
    fn create_match(&mut self, config: MatchConfig) -> String {
        let clients = config.client_tokens.iter().map(|token_hex| {
            let token = Token::from_hex(&token_hex).unwrap();
            self.game_server.get_client(&token)
        }).collect::<Vec<_>>();
    
        let match_ctx = self.game_server.create_match();
        let match_id = gen_match_id();
        self.matches.insert(match_id.clone(),
            MatchData { log: match_ctx.output_stream().clone() }
        );
        tokio::spawn(run_match(clients, match_ctx));
        return match_id;
    }
}

async fn run_match(mut clients: Vec<ClientHandle>, mut match_ctx: MatchCtx) {
    let players = clients.iter_mut().enumerate().map(|(i, client)| {
        let player_token: Token = rand::thread_rng().gen();
        match_ctx.create_player((i+1) as u32, player_token);
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

fn gen_match_id() -> String {
    let id: [u8; 16] = rand::random();
    hex::encode(&id)
}

fn create_match(
    mgr: Arc<Mutex<GameManager>>,
    match_config: MatchConfig,
) -> impl Reply
{
    let mut manager = mgr.lock().unwrap();
    let match_id = manager.create_match(match_config);
    return match_id;
}

fn get_match_log(
    mgr: Arc<Mutex<GameManager>>,
    match_id: String,
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

#[tokio::main]
async fn main() {
    let game_server = GameServer::new();
    // TODO: can we run these on the same port? Would that be desirable?
    tokio::spawn(game_server.run_ws_server("127.0.0.1:8080".to_string()));

    let game_manager = Arc::new(Mutex::new(
        GameManager {
            game_server,
            matches: HashMap::new()
        })
    );

    let create_match_route = warp::path("matches")
        .and(warp::post())
        .and(with_game_manager(game_manager.clone()))
        .and(warp::body::json())
        .map(create_match);
    
    let get_match_route = warp::path("matches")
        .and(warp::get())
        .and(with_game_manager(game_manager.clone()))
        .and(warp::path::param())
        .map(get_match_log);
    
        let routes = create_match_route.or(get_match_route);

    warp::serve(routes).run(([127, 0, 0, 1], 7412)).await;
}
