use futures::stream::futures_unordered::FuturesUnordered;
use futures::{FutureExt, StreamExt};
use mozaic_core::match_context::{MatchCtx, RequestResult};
use tokio::time::Duration;

use serde_json;

use std::collections::HashMap;
use std::convert::TryInto;

pub use planetwars_rules::config::{Config, Map};

use planetwars_rules::protocol as proto;
use planetwars_rules::rules::{PlanetWars as PwState, Dispatch};
use planetwars_rules::serializer as pw_serializer;

pub struct Planetwars {
    match_ctx: MatchCtx,
    state: PwState,
    planet_map: HashMap<String, usize>,
}

impl Planetwars {
    pub fn create(match_ctx: MatchCtx, config: Config) -> Self {
        // TODO: this is kind of hacked together at the moment
        let state = config.create_game(match_ctx.players().len());

        let planet_map = state
            .planets
            .iter()
            .map(|p| (p.name.clone(), p.id))
            .collect();

        Self {
            state,
            planet_map,
            match_ctx,
        }
    }

    pub async fn run(mut self) {
        while !self.state.is_finished() {
            let player_messages = self.prompt_players().await;

            self.state.repopulate();
            for (player_id, turn) in player_messages {
                self.execute_action(player_id, turn);
            }
            self.state.step();

            // Log state
            let state = pw_serializer::serialize(&self.state);
            self.match_ctx.emit(serde_json::to_string(&state).unwrap());
        }
    }

    async fn prompt_players(&mut self) -> Vec<(usize, RequestResult<Vec<u8>>)> {
        // borrow these outside closure to make the borrow checker happy
        let state = &self.state;
        let match_ctx = &mut self.match_ctx;

        self.state
            .players
            .iter()
            .filter(|p| p.alive)
            .map(move |player| {
                let state_for_player =
                    pw_serializer::serialize_rotated(&state, player.id);
                match_ctx
                    .request(
                        player.id.try_into().unwrap(),
                        serde_json::to_vec(&state_for_player).unwrap(),
                        Duration::from_millis(1000),
                    )
                    .map(move |resp| (player.id, resp))
            })
            .collect::<FuturesUnordered<_>>()
            .collect::<Vec<_>>()
            .await
    }

    fn execute_action<'a>(
        &mut self,
        player_num: usize,
        turn: RequestResult<Vec<u8>>,
    ) -> proto::PlayerAction {
        let turn = match turn {
            Err(_timeout) => return proto::PlayerAction::Timeout,
            Ok(data) => data,
        };

        let action: proto::Action = match serde_json::from_slice(&turn) {
            Err(err) => {
                return proto::PlayerAction::ParseError(err.to_string())
            }
            Ok(action) => action,
        };

        let commands = action
            .commands
            .into_iter()
            .map(|command| {
                match self.check_valid_command(player_num, &command) {
                    Ok(dispatch) => {
                        self.state.dispatch(&dispatch);
                        proto::PlayerCommand {
                            command,
                            error: None,
                        }
                    }
                    Err(error) => proto::PlayerCommand {
                        command,
                        error: Some(error),
                    },
                }
            })
            .collect();

        return proto::PlayerAction::Commands(commands);
    }

    fn check_valid_command(
        &self,
        player_num: usize,
        mv: &proto::Command,
    ) -> Result<Dispatch, proto::CommandError> {
        let origin_id = *self
            .planet_map
            .get(&mv.origin)
            .ok_or(proto::CommandError::OriginDoesNotExist)?;

        let target_id = *self
            .planet_map
            .get(&mv.destination)
            .ok_or(proto::CommandError::DestinationDoesNotExist)?;

        if self.state.planets[origin_id].owner() != Some(player_num) {
            return Err(proto::CommandError::OriginNotOwned);
        }

        if self.state.planets[origin_id].ship_count() < mv.ship_count {
            return Err(proto::CommandError::NotEnoughShips);
        }

        if mv.ship_count == 0 {
            return Err(proto::CommandError::ZeroShipMove);
        }

        Ok(Dispatch {
            origin: origin_id,
            target: target_id,
            ship_count: mv.ship_count,
        })
    }
}
