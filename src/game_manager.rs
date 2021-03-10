use std::{collections::HashMap, fs::File, io::{self, Write}, path::{Path, PathBuf}};
use std::sync::{Arc, Mutex};

use futures::{FutureExt, StreamExt, stream};
use io::{BufRead, BufReader};
use mozaic_core::{EventBus, GameServer, MatchCtx, MsgStreamHandle, Token, client_manager::{ClientHandle, ClientMgrHandle}, match_context::PlayerHandle, msg_stream::msg_stream};
use rand::Rng;

use crate::{planetwars::{self, MatchConfig}};

const MATCHES_DIRECTORY: &'static str = "matches";

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
pub enum MatchData {
    InProgress {
        log_stream: MsgStreamHandle<String>
    },
    Finished {
        log_path: PathBuf
    },
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
}

// TODO: this function is very weird.
pub fn create_match<F>(
    game_mgr: Arc<Mutex<GameManager>>,
    tokens: Vec<Token>,
    match_config: MatchConfig,
    callback: F
) -> String
    where F: 'static + Send + Sync + FnOnce(String) -> ()
{
    let mut mgr = game_mgr.lock().unwrap();

    let clients = tokens.iter().map(|token| {
        mgr.game_server.get_client(&token)
    }).collect::<Vec<_>>();

    let match_id = gen_match_id();
    let mut log = msg_stream();
    mgr.matches.insert(match_id.clone(),
        MatchData::InProgress { log_stream: log.clone() }
    );

    println!("Starting match {}", &match_id);

    let cb_match_id = match_id.clone();
    let cb_game_mgr = game_mgr.clone();
    tokio::spawn(run_match(
        clients,
        mgr.game_server.clone(),
        match_config,
        log.clone()).map(move |_| {
            log.terminate();
            let log_path = match_file_path(&cb_match_id);
            let res = write_match_to_disk(&log_path, log);
            if let Err(err) = res {
                eprint!("write match {} failed: {}", cb_match_id, err);
            }
            cb_game_mgr.lock()
                .unwrap()
                .matches
                .insert(cb_match_id.clone(), MatchData::Finished { log_path });
            println!("Finished match {}", &cb_match_id);
            callback(cb_match_id);
        })
    );
    return match_id;
}


pub fn match_file_path(match_id: &str) -> PathBuf {
    let mut path_buf = PathBuf::new();
    path_buf.push(MATCHES_DIRECTORY);
    path_buf.push(match_id);
    path_buf.set_extension("jsonl");
    return path_buf;
}

pub fn write_match_to_disk(path: &Path, log: MsgStreamHandle<String>) -> io::Result<()> {
    let entries = log.to_vec();
    let mut file = File::create(path)?;
    for log_entry in entries {
        file.write_all(log_entry.as_bytes())?;
        file.write_all(b"\n")?;
    }
    Ok(())
}

pub fn read_match_log_from_disk(path: &Path) -> io::Result<Vec<String>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut log = Vec::new();
    for line in reader.lines() {
        log.push(line?);
    }
    Ok(log)
}