use crate::controller::{ControllerClient, GetMuteList, MutePlayer, UnmutePlayer};
use crate::error::*;
use crate::lan::game::{GameEndReason, LanGameInfo};
use crate::node::stream::NodeStreamSender;
use crate::node::NodeInfo;
use flo_net::w3gs::W3GSPacket;
use flo_state::Addr;
use flo_types::node::NodeGameStatus;
use flo_util::chat::{parse_chat_command, ChatCommand};
#[cfg(feature = "blacklist")]
use flo_w3c::blacklist;
use flo_w3c::stats::get_stats;
use flo_w3gs::chat::ChatFromHost;
use flo_w3gs::leave::LeaveReq;
use flo_w3gs::net::W3GSStream;
use flo_w3gs::packet::*;
use flo_w3gs::protocol::action::{OutgoingAction, OutgoingKeepAlive};
use flo_w3gs::protocol::chat::{ChatMessage, ChatToHost};
use flo_w3gs::protocol::constants::PacketTypeId;
use flo_w3gs::protocol::leave::LeaveAck;
use flo_w3gs::protocol::ping::PingFromHost;
use parking_lot::Mutex;
use std::collections::BTreeSet;
use std::time::Duration;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::watch::Receiver as WatchReceiver;
use tokio::time::interval;

#[derive(Debug)]
pub enum GameResult {
  Disconnected,
  Leave,
}

pub struct GameHandler<'a> {
  info: &'a LanGameInfo,
  node: &'a NodeInfo,
  w3gs_stream: &'a mut W3GSStream,
  node_stream: &'a mut NodeStreamSender,
  status_rx: &'a mut WatchReceiver<Option<NodeGameStatus>>,
  w3gs_tx: &'a mut Sender<Packet>,
  w3gs_rx: &'a mut Receiver<Packet>,
  client: &'a mut Addr<ControllerClient>,
  muted_players: BTreeSet<u8>,
  end_reason: &'a Mutex<Option<GameEndReason>>,
}

impl<'a> GameHandler<'a> {
  pub fn new(
    info: &'a LanGameInfo,
    node: &'a NodeInfo,
    stream: &'a mut W3GSStream,
    node_stream: &'a mut NodeStreamSender,
    status_rx: &'a mut WatchReceiver<Option<NodeGameStatus>>,
    w3gs_tx: &'a mut Sender<Packet>,
    w3gs_rx: &'a mut Receiver<Packet>,
    client: &'a mut Addr<ControllerClient>,
    end_reason: &'a Mutex<Option<GameEndReason>>,
  ) -> Self {
    GameHandler {
      info,
      node,
      w3gs_stream: stream,
      node_stream,
      status_rx,
      w3gs_tx,
      w3gs_rx,
      client,
      muted_players: BTreeSet::new(),
      end_reason,
    }
  }

  pub async fn run(
    &mut self,
    deferred_in_packets: Vec<Packet>,
    deferred_out_packets: Vec<Packet>,
  ) -> Result<GameResult> {
    let mute_list = if let Ok(v) = self.client.send(GetMuteList).await {
      v
    } else {
      vec![]
    };
    let mut muted_names = vec![];
    #[cfg(feature = "blacklist")]
    let mut blacklisted = vec![];
    for p in &self.info.slot_info.player_infos {
      if mute_list.contains(&p.player_id) {
        muted_names.push(p.name.clone());
        self.muted_players.insert(p.slot_player_id);
      }
      #[cfg(feature = "blacklist")]
      if let Some(r) = blacklist::read(&p.name).unwrap_or(None) {
        blacklisted.push(format!("{} for {}", p.name.clone(), r));
      }
    }
    if !muted_names.is_empty() {
      self.send_chats_to_self(
        self.info.slot_info.my_slot_player_id,
        vec![format!("Auto muted: {}", muted_names.join(", "))],
      )
    }
    #[cfg(feature = "blacklist")]
    if !blacklisted.is_empty() {
      self.send_chats_to_self(
        self.info.slot_info.my_slot_player_id,
        vec![format!("Blacklisted: {}", blacklisted.join(", "))],
      )
    }

    for pkt in deferred_in_packets {
      tracing::warn!("deferred in packet: {:?}", pkt.type_id());
      self.handle_incoming_w3gs(pkt).await?;
    }

    for pkt in deferred_out_packets {
      tracing::warn!("deferred out packet: {:?}", pkt.type_id());
      self.node_stream.send_w3gs(pkt).await?;
    }

    let mut ping = interval(Duration::from_secs(15));
    let ping_packet = Packet::simple(PingFromHost::with_payload(0))?;

    loop {
      tokio::select! {
        _ = ping.tick() => {
          self.w3gs_stream.send(ping_packet.clone()).await?;
        }
        next = self.w3gs_stream.recv() => {
          let pkt = match next {
            Ok(pkt) => pkt,
            Err(err) => {
              tracing::error!("game connection: {}", err);
              return Ok(GameResult::Disconnected)
            },
          };
          if let Some(pkt) = pkt {
            if pkt.type_id() == LeaveAck::PACKET_TYPE_ID {
              tracing::info!("game leave ack received");
              self.w3gs_stream.send(Packet::simple(LeaveAck)?).await?;
              self.w3gs_stream.flush().await?;
              return Ok(GameResult::Leave)
            }

            self.handle_game_packet(pkt).await?;
          } else {
            tracing::info!("game stream closed");
            return Ok(GameResult::Disconnected)
          }
        }
        changed = self.status_rx.changed() => {
          let next =
            if changed.is_ok() {
              self.status_rx.borrow().clone()
            } else {
              return Err(Error::TaskCancelled(anyhow::format_err!("game status tx dropped")))
            };
          match next {
            Some(status) => {
              self.handle_game_status_change(status).await?;
            },
            None => {},
          }
        }
        next = self.w3gs_rx.recv() => {
          if let Some(pkt) = next {
            self.handle_incoming_w3gs(pkt).await?;
          } else {
            return Err(Error::TaskCancelled(anyhow::format_err!("W3GS tx dropped")))
          }
        }
      }
    }
  }

  #[inline]
  async fn handle_incoming_w3gs(&mut self, pkt: Packet) -> Result<()> {
    match pkt.type_id() {
      OutgoingKeepAlive::PACKET_TYPE_ID => {}
      OutgoingAction::PACKET_TYPE_ID => {}
      ChatFromHost::PACKET_TYPE_ID => {
        if !self.muted_players.is_empty() {
          let pkt: ChatFromHost = pkt.decode_simple()?;
          if let ChatToHost {
            message: ChatMessage::Scoped { .. },
            ..
          } = pkt.0
          {
            if self.muted_players.contains(&pkt.from_player()) {
              return Ok(());
            }
          }
        }
      }
      _other => {}
    }

    // tracing::debug!("send: {:?}", pkt.type_id());

    self.w3gs_stream.send(pkt).await?;
    Ok(())
  }

  async fn handle_game_status_change(&mut self, status: NodeGameStatus) -> Result<()> {
    tracing::debug!("game status changed: {:?}", status);
    Ok(())
  }

  async fn handle_game_packet(&mut self, pkt: Packet) -> Result<()> {
    match pkt.type_id() {
      PacketTypeId::PongToHost => return Ok(()),
      ChatToHost::PACKET_TYPE_ID => {
        let pkt: ChatToHost = pkt.decode_simple()?;
        match pkt.message {
          ChatMessage::Scoped { message, .. } => {
            if let Some(cmd) = parse_chat_command(message.as_bytes()) {
              if self.handle_chat_command(cmd) {
                return Ok(());
              }
            }
          }
          _ => {}
        }
      }
      OutgoingKeepAlive::PACKET_TYPE_ID => {}
      OutgoingAction::PACKET_TYPE_ID => {}
      PacketTypeId::DropReq => {}
      PacketTypeId::LeaveReq => {
        let payload: LeaveReq = pkt.decode_simple()?;
        tracing::info!("request to leave received: {:?}", payload.reason());
        self
          .end_reason
          .lock()
          .replace(GameEndReason::LeaveReq(payload.reason()));

        if let Err(err) = self.node_stream.send_w3gs(pkt).await {
          tracing::error!("report request to leave: {}", err);
        }
        self.w3gs_stream.send(W3GSPacket::simple(LeaveAck)?).await?;

        return Ok(());
      }
      _ => {
        tracing::debug!("unknown game packet: {:?}", pkt.type_id());
      }
    }

    self.node_stream.send_w3gs(pkt).await?;

    Ok(())
  }

  fn handle_chat_command(&mut self, cmd: ChatCommand) -> bool {
    match cmd.raw() {
      "flo" => {
        let messages = vec![
          "-game: print game information.".to_string(),
          "-muteall: Mute all players.".to_string(),
          "-muteopps: Mute all opponents.".to_string(),
          "-unmuteall: Unmute all players.".to_string(),
          "-mute/mutef: Mute your opponent (1v1), or display a player list.".to_string(),
          "-mute/mutef <ID>: Mute a player.".to_string(),
          "-unmute/unmutef: Unmute your opponent (1v1), or display a player list.".to_string(),
          "-unmute/unmutef <ID>: Unmute a player.".to_string(),
          "-rtt: Print round-trip time information.".to_string(),
          "-stats: Print opponent/opponents statistics.".to_string(),
          "-stats <ID>: Print player statistics, or display a player list.".to_string(),
        ];
        self.send_chats_to_self(self.info.slot_info.my_slot_player_id, messages)
      }
      "game" => {
        let mut messages = vec![
          format!(
            "Game: {} (#{})",
            self.info.game.name, self.info.game.game_id
          ),
          format!(
            "Server: {}, {}, {} (#{})",
            self.node.name, self.node.location, self.node.country_id, self.node.id
          ),
          "Players:".to_string(),
        ];

        for slot in &self.info.game.slots {
          if let Some(ref player) = slot.player.as_ref() {
            messages.push(format!(
              "  {}: Team {}, {:?}",
              player.name, slot.settings.team, slot.settings.race
            ));
          }
        }

        self.send_chats_to_self(self.info.slot_info.my_slot_player_id, messages)
      }
      "muteall" => {
        let targets: Vec<u8> = self
          .info
          .slot_info
          .player_infos
          .iter()
          .filter_map(|slot| {
            if slot.slot_player_id == self.info.slot_info.my_slot_player_id {
              return None;
            }
            Some(slot.slot_player_id)
          })
          .collect();
        self.muted_players.extend(targets);
        self.send_chats_to_self(
          self.info.slot_info.my_slot_player_id,
          vec![format!("All players muted.")],
        );
      }
      "muteopps" => {
        let my_team = self.info.slot_info.my_slot.team;
        let targets: Vec<u8> = self
          .info
          .slot_info
          .player_infos
          .iter()
          .filter_map(|slot| {
            if slot.slot_player_id == self.info.slot_info.my_slot_player_id {
              return None;
            }
            if self.info.game.slots[slot.slot_index].settings.team == my_team as i32 {
              return None;
            }
            Some(slot.slot_player_id)
          })
          .collect();
        self.muted_players.extend(targets);
        self.send_chats_to_self(
          self.info.slot_info.my_slot_player_id,
          vec![format!("All opponents muted.")],
        );
      }
      "unmuteall" => {
        self.muted_players.clear();
        self.send_chats_to_self(
          self.info.slot_info.my_slot_player_id,
          vec![format!("All players un-muted.")],
        );
      }
      #[cfg(feature = "blacklist")]
      "blacklisted" => {
        if let Ok(b) = blacklist::blacklisted() {
          self.send_chats_to_self(self.info.slot_info.my_slot_player_id, vec![b]);
        }
      }
      cmd if cmd.starts_with("stats") => {
        let cmd = cmd.trim_end();
        let players = &self.info.slot_info.player_infos;
        let solo = players.len() == 2;
        if cmd == "stats" {
          let my_team = self.info.slot_info.my_slot.team;
          let targets: Vec<(String, u32)> = players
            .iter()
            .filter_map(|slot| {
              if slot.slot_player_id == self.info.slot_info.my_slot_player_id {
                return None;
              }
              if self.info.game.slots[slot.slot_index].settings.team == my_team as i32 {
                return None;
              }
              Some((
                slot.name.clone(),
                self.info.game.slots[slot.slot_index].settings.race as u32,
              ))
            })
            .collect();
          if !targets.is_empty() {
            self.send_stats_to_self(self.info.slot_info.my_slot_player_id, targets, solo);
          }
        } else {
          let id_or_name = &cmd["stats ".len()..];
          if let Ok(id) = id_or_name.parse::<u8>() {
            let targets: Vec<(String, u32)> = players
              .iter()
              .filter_map(|slot| {
                if slot.slot_player_id == id {
                  Some((
                    slot.name.clone(),
                    self.info.game.slots[slot.slot_index].settings.race as u32,
                  ))
                } else {
                  None
                }
              })
              .collect();
            if !targets.is_empty() {
              self.send_stats_to_self(self.info.slot_info.my_slot_player_id, targets, solo);
            } else {
              let mut msgs = vec![format!("Type `-stats <ID>` to get stats for:")];
              for slot in &self.info.slot_info.player_infos {
                msgs.push(format!(
                  " ID={} {}",
                  slot.slot_player_id,
                  slot.name.as_str()
                ));
              }
              self.send_chats_to_self(self.info.slot_info.my_slot_player_id, msgs);
            }
          } else {
            let targets: Vec<(String, u32)> = players
              .iter()
              .filter_map(|slot| {
                if slot
                  .name
                  .to_lowercase()
                  .starts_with(&id_or_name.to_lowercase())
                {
                  Some((
                    slot.name.clone(),
                    self.info.game.slots[slot.slot_index].settings.race as u32,
                  ))
                } else {
                  None
                }
              })
              .collect();
            if !targets.is_empty() {
              self.send_stats_to_self(self.info.slot_info.my_slot_player_id, targets, solo);
            } else {
              let mut msgs = vec![format!("Type `-stats <ID>` to get stats for:")];
              for slot in &self.info.slot_info.player_infos {
                msgs.push(format!(
                  " ID={} {}",
                  slot.slot_player_id,
                  slot.name.as_str()
                ));
              }
              self.send_chats_to_self(self.info.slot_info.my_slot_player_id, msgs);
            }
          }
        }
      }
      #[cfg(feature = "blacklist")]
      cmd if cmd.starts_with("blacklist") || cmd.starts_with("unblacklist") => {
        let unblacklist = cmd.starts_with("unblacklist");
        let cmd = cmd.trim_end();
        let players = &self.info.slot_info.player_infos;
        let args = if unblacklist {
          &cmd["unblacklist ".len()..]
        } else {
          &cmd["blacklist ".len()..]
        };
        if args.is_empty() {
          let mut msgs = vec![format!("Type `-blacklist <ID>` to blacklist:")];
          for slot in &self.info.slot_info.player_infos {
            msgs.push(format!(
              " ID={} {}",
              slot.slot_player_id,
              slot.name.as_str()
            ));
          }
          self.send_chats_to_self(self.info.slot_info.my_slot_player_id, msgs);
        } else {
          let args_split: Vec<&str> = args.split_whitespace().collect();
          let id_or_name = args_split[0];
          let reason = if args_split.len() > 1 {
            args_split
              .into_iter()
              .skip(1)
              .collect::<Vec<&str>>()
              .join(" ")
          } else {
            "no reason".to_string()
          };
          if let Ok(id) = id_or_name.parse::<u8>() {
            let targets: Vec<String> = players
              .iter()
              .filter_map(|slot| {
                if slot.slot_player_id == id {
                  Some(slot.name.clone())
                } else {
                  None
                }
              })
              .collect();
            if !targets.is_empty() {
              if unblacklist {
                if blacklist::unblacklist(targets[0].as_str()).is_ok() {
                  self.send_chats_to_self(
                    self.info.slot_info.my_slot_player_id,
                    vec![format!("{} un-blacklisted", &targets[0])],
                  );
                }
              } else {
                if blacklist::blacklist(targets[0].as_str(), &reason).is_ok() {
                  self.send_chats_to_self(
                    self.info.slot_info.my_slot_player_id,
                    vec![format!("{} blacklisted", &targets[0])],
                  );
                }
              }
            }
          } else {
            let targets: Vec<String> = players
              .iter()
              .filter_map(|slot| {
                if slot
                  .name
                  .to_lowercase()
                  .starts_with(&id_or_name.to_lowercase())
                {
                  Some(slot.name.clone())
                } else {
                  None
                }
              })
              .collect();
            if !targets.is_empty() {
              if unblacklist {
                if blacklist::unblacklist(targets[0].as_str()).is_ok() {
                  self.send_chats_to_self(
                    self.info.slot_info.my_slot_player_id,
                    vec![format!("{} un-blacklisted", &targets[0])],
                  );
                }
              } else {
                if blacklist::blacklist(targets[0].as_str(), &reason).is_ok() {
                  self.send_chats_to_self(
                    self.info.slot_info.my_slot_player_id,
                    vec![format!("{} blacklisted", &targets[0])],
                  );
                }
              }
            }
          }
        }
      }
      cmd if cmd.starts_with("mute") => {
        let targets: Vec<(u8, &str, i32)> = self
          .info
          .slot_info
          .player_infos
          .iter()
          .filter_map(|slot| {
            if slot.slot_player_id == self.info.slot_info.my_slot_player_id {
              return None;
            }
            if !self.muted_players.contains(&slot.slot_player_id) {
              Some((slot.slot_player_id, slot.name.as_str(), slot.player_id))
            } else {
              None
            }
          })
          .collect();

        let cmd = cmd.trim_end();
        if cmd == "mute" || cmd == "mutef" {
          let forever = cmd == "mutef";
          match targets.len() {
            0 => {
              self.send_chats_to_self(
                self.info.slot_info.my_slot_player_id,
                vec![format!("You have silenced all the players.")],
              );
              return true;
            }
            1 => {
              let (slot_player_id, name, player_id) = &targets[0];
              self.muted_players.insert(*slot_player_id);
              if forever {
                self.save_mute(*player_id, name.to_string(), true);
              } else {
                self.send_chats_to_self(
                  self.info.slot_info.my_slot_player_id,
                  vec![format!("Muted: {}", targets[0].1)],
                );
              }
            }
            _ => {
              let mut msgs = vec![format!("Type `-mute or -mutef <ID>` to mute a player:")];
              for (id, name, _) in targets {
                msgs.push(format!(" ID={} {}", id, name));
              }
              self.send_chats_to_self(self.info.slot_info.my_slot_player_id, msgs);
            }
          }
        } else {
          let forever = cmd.starts_with("mutef");
          let id = if forever {
            &cmd["mutef ".len()..]
          } else {
            &cmd["mute ".len()..]
          };
          if let Ok(id) = id.parse::<u8>() {
            if id == self.info.slot_info.my_slot_player_id {
              self.send_chats_to_self(
                self.info.slot_info.my_slot_player_id,
                vec![format!("You cannot mute yourself.")],
              );
              return true;
            }

            if let Some(info) = self
              .info
              .slot_info
              .player_infos
              .iter()
              .find(|info| info.slot_player_id == id)
            {
              self.muted_players.insert(id);

              if forever {
                self.save_mute(info.player_id, info.name.clone(), true);
              } else {
                self.send_chats_to_self(
                  self.info.slot_info.my_slot_player_id,
                  vec![format!("Muted: {}", info.name)],
                );
              }
            } else {
              self.send_chats_to_self(self.info.slot_info.my_slot_player_id, {
                let mut msgs = vec![format!("Invalid player id. Players:")];
                for (id, name, _) in targets {
                  msgs.push(format!(" ID={} {}", id, name));
                }
                msgs
              });
            }
          } else {
            self.send_chats_to_self(
              self.info.slot_info.my_slot_player_id,
              vec![format!("Invalid syntax. Example: -mute 1")],
            );
          }
        }
      }
      cmd if cmd.starts_with("unmute") => {
        let targets: Vec<(u8, &str, i32)> = self
          .muted_players
          .iter()
          .cloned()
          .filter_map(|id| {
            if id == self.info.slot_info.my_slot_player_id {
              return None;
            }
            self
              .info
              .slot_info
              .player_infos
              .iter()
              .find(|info| info.slot_player_id == id)
              .map(|info| (info.slot_player_id, info.name.as_str(), info.player_id))
          })
          .collect();

        let cmd = cmd.trim_end();
        if cmd == "unmute" || cmd == "unmutef" {
          let forever = cmd == "unmutef";
          match targets.len() {
            0 => {
              self.send_chats_to_self(
                self.info.slot_info.my_slot_player_id,
                vec![format!("No player to unmute.")],
              );
              return true;
            }
            1 => {
              self.muted_players.remove(&targets[0].0);

              if forever {
                self.save_mute(targets[0].2, targets[0].1.to_string(), false);
              } else {
                self.send_chats_to_self(
                  self.info.slot_info.my_slot_player_id,
                  vec![format!("Un-muted: {}", targets[0].1)],
                );
              }
            }
            _ => {
              let mut msgs = vec![format!("Type `-unmute <ID>` to unmute a player:")];
              for (id, name, _) in targets {
                msgs.push(format!(" ID={} {}", id, name));
              }
              self.send_chats_to_self(self.info.slot_info.my_slot_player_id, msgs);
            }
          }
        } else {
          let forever = cmd.starts_with("unmutef");
          let id = if forever {
            &cmd["unmutef ".len()..]
          } else {
            &cmd["unmute ".len()..]
          };
          if let Some(id) = id.parse::<u8>().ok() {
            if let Some((name, player_id)) = targets
              .iter()
              .find(|info| info.0 == id)
              .map(|info| (info.1, info.2))
            {
              self.muted_players.remove(&id);

              if forever {
                self.save_mute(player_id, name.to_string(), false);
              } else {
                self.send_chats_to_self(
                  self.info.slot_info.my_slot_player_id,
                  vec![format!("Un-muted: {}", name)],
                );
              }
            } else {
              self.send_chats_to_self(self.info.slot_info.my_slot_player_id, {
                let mut msgs = vec![format!("Invalid player id. Muted players:")];
                for (id, name, _) in targets {
                  msgs.push(format!(" ID={} {}", id, name));
                }
                msgs
              });
            }
          } else {
            self.send_chats_to_self(
              self.info.slot_info.my_slot_player_id,
              vec![format!("Invalid syntax. Example: -unmute 1")],
            );
          }
        }
      }
      _ => {
        // unknown command treats like regular chat message
        return false;
      }
    }
    true
  }

  fn send_stats_to_self(&self, player_id: u8, targets: Vec<(String, u32)>, solo: bool) {
    let mut tx = self.w3gs_tx.clone();
    tokio::spawn(async move {
      for (name, race) in targets {
        if let Ok(Ok(target_stats_results)) =
          tokio::task::spawn_blocking(move || get_stats(name.as_str(), race, solo)).await
        {
          send_chats_to_self(&mut tx, player_id, vec![target_stats_results]).await
        }
      }
    });
  }

  fn send_chats_to_self(&self, player_id: u8, messages: Vec<String>) {
    let mut tx = self.w3gs_tx.clone();
    tokio::spawn(async move { send_chats_to_self(&mut tx, player_id, messages).await });
  }

  fn save_mute(&self, player_id: i32, name: String, muted: bool) {
    let mut tx = self.w3gs_tx.clone();
    let client = self.client.clone();
    let my_slot_player_id = self.info.slot_info.my_slot_player_id;
    tokio::spawn(async move {
      let action = if muted { "Muted" } else { "Un-muted" };
      let send = if muted {
        client.send(MutePlayer { player_id }).await
      } else {
        client.send(UnmutePlayer { player_id }).await
      }
      .map_err(Error::from);
      if let Err(err) = send.and_then(std::convert::identity) {
        tracing::error!("save mute failed: {}", err);
        send_chats_to_self(
          &mut tx,
          my_slot_player_id,
          vec![format!("{} temporary: {}", action, name)],
        )
        .await;
      } else {
        send_chats_to_self(
          &mut tx,
          my_slot_player_id,
          vec![format!("{} forever: {}", action, name)],
        )
        .await;
      }
    });
  }
}

async fn send_chats_to_self(tx: &mut Sender<Packet>, player_id: u8, messages: Vec<String>) {
  for message in messages {
    match Packet::simple(ChatFromHost::private_to_self(player_id, message)) {
      Ok(pkt) => {
        tx.send(pkt).await.ok();
      }
      Err(err) => {
        tracing::error!("encode chat packet: {}", err);
      }
    }
  }
}
