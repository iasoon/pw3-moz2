#![feature(async_closure)]

use mozaic_core::match_context::EventBus;
use std::collections::HashMap;
use mozaic_core::msg_stream::MsgStreamHandle;
use serde_bytes::ByteBuf;
use tokio::io::AsyncWriteExt;
use tokio::prelude::AsyncWrite;
use tokio::prelude::AsyncRead;
use tokio::io::AsyncReadExt;
use mozaic_core::player_supervisor::RequestMessage;
use mozaic_core::msg_stream::MsgStreamReader;
use mozaic_core::msg_stream::msg_stream;
use mozaic_core::match_context::{self as match_ctx, MatchCtx};
use serde::{Deserialize, Serialize};
use futures::{select, StreamExt, FutureExt};
use std::io;

use tokio::sync::mpsc;

use mozaic_core::utils::StreamSet;
use planetwars_rules::PwConfig;

mod planetwars;

#[tokio::main]
async fn main() {
    let (tx, rx) = mpsc::channel(32);
    let mut h = Handler {
        player_streams: StreamSet::new(),
        match_event_bus: HashMap::new(),
        tx,
        rx: rx.fuse(),
    };

    let res = h.run().await;
    if let Err(err) = res {
        if err.kind() != io::ErrorKind::UnexpectedEof {
            panic!("error in stdio handler: {}", err);
        }
    }
}

struct Handler {
    player_streams: StreamSet<PlayerUid, MsgStreamReader<RequestMessage>>,
    match_event_bus: HashMap<String, EventBus>,
    rx: futures::stream::Fuse<mpsc::Receiver<HandlerMsg>>,
    tx: mpsc::Sender<HandlerMsg>,
}

enum HandlerMsg {
    MatchComplete { id: String, log: MsgStreamHandle<String> },
}

async fn read_frame<R>(stream: &mut R) -> io::Result<Vec<u8>>
    where R: AsyncRead + Unpin
{
    let len = stream.read_u32().await?;
    let mut buf = vec![0; len as usize];
    stream.read_exact(&mut buf).await?;
    return Ok(buf);
}

async fn write_frame<W>(stream: &mut W, buf: Vec<u8>) -> io::Result<()>
    where W: AsyncWrite + Unpin
{
    stream.write_u32(buf.len() as u32).await?;
    stream.write_all(&buf).await?;
    stream.flush().await
}

impl Handler {
    async fn run(&mut self) -> io::Result<()> {
        let mut stdin = tokio::io::stdin();
        let mut stdout = tokio::io::stdout();

        loop {
            select!(
                read_res = read_frame(&mut stdin).fuse() => {
                    let frame = read_res?;
                    let msg = rmp_serde::from_slice(&frame).expect("invalid message");
                    self.handle_message(msg);
                }
                item = self.player_streams.next() => {
                    let (player_uid, req) = item.unwrap();
                    let content: &Vec<u8> = req.content.as_ref();
                    let msg = ServerMessage::PlayerRequest(
                        PlayerRequest {
                            player_id: player_uid.player_id,
                            match_id: player_uid.match_id,
                            request_id: req.request_id,
                            // TODO: dont clone here
                            content: ByteBuf::from(content.clone()),    
                        }
                    );
                    let frame = rmp_serde::to_vec_named(&msg).unwrap();
                    write_frame(&mut stdout, frame).await?;
                }
                item = self.rx.next() => {
                    let msg = item.unwrap();
                    self.handle_event(msg, &mut stdout).await?;
                }
            )
        }
    }

    fn start_match(&mut self, m: StartMatch) {
        let event_bus = msg_stream();
        let log = msg_stream();

        self.match_event_bus.insert(m.match_id.clone(), event_bus.clone());

        // create players
        let players = (0..m.num_players).map(|player_num| {
            let stream = msg_stream();
            let player_id = (player_num + 1) as u32;
            let player_uid = PlayerUid {
                match_id: m.match_id.clone(),
                player_id
            };
            self.player_streams.push(player_uid, stream.reader());

            ((player_num+1) as u32, stream)
        }).collect();

        let match_ctx = MatchCtx::new(event_bus, players, log.clone());
        let pw_match = planetwars::PwMatch::create(match_ctx, m.config);

        let mut tx = self.tx.clone();
        let id = m.match_id.clone();
        tokio::spawn(pw_match.run().then(async move |_| {
            tx.send(HandlerMsg::MatchComplete {
                id,
                log,
            }).await
        }));
    }

    fn handle_response(&mut self, resp: PlayerResponse) {
        let event_bus = self.match_event_bus.get_mut(&resp.match_id)
            .unwrap_or_else(|| panic!("match does not exist"));
        event_bus.write(match_ctx::GameEvent::PlayerResponse(
            match_ctx::PlayerResponse {
                player_id: resp.player_id,
                request_id: resp.request_id,
                response: Ok(resp.content.into_vec()),
            })
        );
    }

    fn handle_message(&mut self, m: CtrlMsg) {
        match m {
            CtrlMsg::StartMatch(start_match) => {
                self.start_match(start_match);
            }
            CtrlMsg::PlayerResponse(player_response) => {
                self.handle_response(player_response);
            }
        }
    }

    async fn handle_event<W>(&mut self, event: HandlerMsg, stdout: &mut W)
        -> io::Result<()>
        where W: AsyncWrite + Unpin
    {
        match event {
            HandlerMsg::MatchComplete { id, log } => {
                // cleanup
                self.match_event_bus.remove(&id);
                let msg = ServerMessage::MatchFinished(
                    MatchFinished {
                        match_id: id,
                        match_log: log.to_vec()
                            .into_iter()
                            .map(|s| s.as_ref().clone())
                            .collect(),
                    }
                );
                let buf = rmp_serde::to_vec_named(&msg).unwrap();
                write_frame(stdout, buf).await
            }
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag="action")]
#[serde(rename_all="snake_case")]
enum CtrlMsg {
    StartMatch(StartMatch),
    PlayerResponse(PlayerResponse),
}


#[derive(Serialize, Deserialize)]
struct StartMatch {
    match_id: String,
    num_players: usize,
    config: PwConfig,
}

#[derive(Serialize, Deserialize)]
struct PlayerResponse {
    match_id: String,
    player_id: u32,
    request_id: u32,
    content: ByteBuf,
}

#[derive(Serialize, Deserialize)]
#[serde(tag="type")]
#[serde(rename_all="snake_case")]
enum ServerMessage {
    PlayerRequest(PlayerRequest),
    MatchFinished(MatchFinished),
}


#[derive(Serialize, Deserialize)]
struct PlayerRequest {
    match_id: String,
    player_id: u32,
    request_id: u32,
    content: ByteBuf,
}

#[derive(Serialize, Deserialize)]
struct MatchFinished {
    match_id: String,
    match_log: Vec<String>,
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Clone, Hash)]
struct PlayerUid {
    pub match_id: String,
    pub player_id: u32,
}
