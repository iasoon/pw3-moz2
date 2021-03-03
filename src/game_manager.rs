use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures::{FutureExt, StreamExt, stream};
use mozaic_core::{EventBus, GameServer, MatchCtx, MsgStreamHandle, Token, client_manager::{ClientHandle, ClientMgrHandle}, match_context::PlayerHandle, msg_stream::msg_stream};
use rand::Rng;

use crate::{planetwars::{self, MatchConfig}};

pub struct GameManager {
    game_server: GameServer,
    matches: HashMap<String, MatchData>
}

impl GameManager {
    pub fn init(addr: String) -> Arc<Mutex<GameManager>> {
        let game_server = GameServer::new();

        tokio::spawn(game_server.run_ws_server(addr));

        let game_manager = GameManager {
            game_server,
            matches: HashMap::new(),
        };

        return Arc::new(Mutex::new(game_manager));
    }

    pub fn create_match<F>(&mut self, tokens: Vec<Token>, match_config: MatchConfig, cb: F) -> String
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

    pub fn get_match_data<'a>(&'a self, match_id: &str) -> Option<&'a MatchData> {
        self.matches.get(match_id)
    }

    pub fn list_matches(&self) -> Vec<String> {
        self.matches.keys().cloned().collect()
    }

    // TODO: find a cleaner way
    pub fn client_manager_mut(&mut self) -> &mut ClientMgrHandle {
        self.game_server.client_manager_mut()
    }

    pub fn client_connected(&self, token: &Token) -> bool {
        self.game_server.client_manager().is_connected(token)
    }
}

pub struct MatchData {
    pub log: MsgStreamHandle<String>,
}

fn gen_match_id() -> String {
    let id: [u8; 16] = rand::random();
    hex::encode(&id)
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
