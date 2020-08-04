pub mod db;
mod slots;
pub mod token;
mod types;

use s2_grpc_utils::S2ProtoPack;

use flo_net::proto;

use crate::error::*;
use crate::game::db::{LeaveGameParams, UpdateGameSlotSettingsParams};
use crate::state::LobbyStateRef;
pub use slots::Slots;
pub use types::*;

pub async fn join_game(state: LobbyStateRef, game_id: i32, player_id: i32) -> Result<Game> {
  use crate::game::db::JoinGameParams;

  let params = JoinGameParams { game_id, player_id };

  let game = {
    let mut player_guard = state.mem.lock_player_state(player_id).await;
    if player_guard.joined_game_id().is_some() {
      return Err(Error::MultiJoin.into());
    }

    let mut game_guard = state
      .mem
      .lock_game_state(params.game_id)
      .await
      .ok_or_else(|| Error::GameNotFound)?;
    let game = state
      .db
      .exec(move |conn| {
        let id = params.game_id;
        crate::game::db::join(conn, params)?;
        crate::game::db::get_full(conn, id)
      })
      .await
      .map_err(Error::from)?;

    player_guard.join_game(game.id);
    game_guard.add_player(player_id);
    let update = player_guard.get_session_update();
    if let Some(sender) = player_guard.get_sender_mut() {
      let next_game = game.clone().into_packet();
      sender.with_buf(move |buf| {
        buf.update_session(update);
        buf.set_game(next_game);
      });
    }
    game
  };

  {
    let slot_info = game
      .get_player_slot_info(player_id)
      .ok_or_else(|| Error::PlayerSlotNotFound)?;
    let player: proto::flo_connect::PlayerInfo = slot_info.player.clone().pack()?;

    // send notification to other players in this game
    let players = game.get_player_ids();
    let mut senders = state.mem.get_player_senders(&players);
    for sender in senders.values_mut() {
      if sender.player_id() != player_id {
        sender.with_buf(|buf| {
          buf.add_player_enter(
            game.id,
            player.clone(),
            slot_info.slot_index as i32,
            slot_info.slot.settings.clone().into_packet(),
          )
        });
      }
    }
  }

  Ok(game)
}

pub async fn leave_game(state: LobbyStateRef, game_id: i32, player_id: i32) -> Result<()> {
  let mut player_guard = state.mem.lock_player_state(player_id).await;

  let player_state_game_id = if let Some(id) = player_guard.joined_game_id() {
    id
  } else {
    return Ok(());
  };

  if player_state_game_id != game_id {
    tracing::warn!("player joined game id mismatch: player_id = {}, player_state_game_id = {}, params.game_id = {}", 
        player_id,
        player_state_game_id,
        game_id
      );
  }

  let mut game_guard = state
    .mem
    .lock_game_state(player_state_game_id)
    .await
    .ok_or_else(|| Error::GameNotFound)?;

  let slots = state
    .db
    .exec(move |conn| {
      crate::game::db::leave(
        conn,
        LeaveGameParams {
          player_id,
          game_id: player_state_game_id,
        },
      )
    })
    .await?;

  player_guard.leave_game();
  game_guard.remove_player(player_id);

  // send to self
  let update = player_guard.get_session_update();
  if let Some(sender) = player_guard.get_sender_mut() {
    sender.with_buf(move |buf| {
      buf.update_session(update);
    });
  }

  let player_ids: Vec<i32> = slots
    .iter()
    .filter_map(|s| s.player.as_ref().map(|p| p.id))
    .collect();

  let mut senders = state.mem.get_player_senders(&player_ids);

  for sender in senders.values_mut() {
    sender.with_buf(|buf| {
      buf.add_player_leave(
        player_state_game_id,
        player_id,
        proto::flo_connect::PlayerLeaveReason::Left,
      )
    });
  }

  Ok(())
}

pub async fn update_game_slot_settings(
  state: LobbyStateRef,
  game_id: i32,
  player_id: i32,
  settings: SlotSettings,
) -> Result<Vec<Slot>> {
  let game_guard = state
    .mem
    .lock_game_state(game_id)
    .await
    .ok_or_else(|| Error::GameNotFound)?;

  if !game_guard.has_player(player_id) {
    return Err(Error::PlayerNotInGame.into());
  }

  let slots = state
    .db
    .exec(move |conn| {
      crate::game::db::update_slot_settings(
        conn,
        UpdateGameSlotSettingsParams {
          game_id,
          player_id,
          settings,
        },
      )
    })
    .await?;

  let index = slots
    .iter()
    .position(|s| {
      s.player
        .as_ref()
        .map(|p| p.id == player_id)
        .unwrap_or(false)
    })
    .ok_or_else(|| Error::PlayerSlotNotFound)?;

  let slot_index = index as i32;
  let settings: proto::flo_connect::SlotSettings = slots[index].settings.clone().pack()?;

  let players = game_guard.players().to_vec();
  drop(game_guard);

  let mut senders = state.mem.get_player_senders(&players);
  for sender in senders.values_mut() {
    sender.with_buf(|buf| buf.add_slot_update(game_id, slot_index, settings.clone()))
  }

  Ok(slots)
}
