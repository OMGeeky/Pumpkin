use std::{borrow::Cow, net::SocketAddr, time::Duration};

use pumpkin_data::{
    item::Item,
    item_stack::ItemStack,
    packet::{
        CURRENT_MC_VERSION,
        clientbound::{config as c_config, login as c_login, play as c_play},
    },
};
use pumpkin_protocol::{
    ClientPacket, ConnectionState, PositionFlag, RawPacket, ServerPacket, VarInt,
    codec::item_stack_seralizer::{
        ItemComponentHash, ItemStackHash, ItemStackSerializer, OptionalItemStackHash,
    },
    java::{
        client::play::{CKeepAlive, CPlayerPosition, CSpawnEntity},
        packet_decoder::TCPNetworkDecoder,
        packet_encoder::TCPNetworkEncoder,
        server::{
            config::{SAcknowledgeFinishConfig, SClientInformationConfig, SKnownPacks},
            handshake::SHandShake,
            login::{SLoginAcknowledged, SLoginStart},
            play::{
                Action, ActionType, SAttack, SChangeGameMode, SChatCommand, SChunkBatch,
                SClickSlot, SClientCommand, SClientTickEnd, SCloseContainer, SConfirmTeleport,
                SContainerButtonClick, SContainerSlotStateChanged, SInteract, SKeepAlive,
                SPickItemFromBlock, SPlaceRecipe, SPlayPingRequest, SPlayerAbilities,
                SPlayerAction, SPlayerCommand, SPlayerInput, SPlayerLoaded, SPlayerPosition,
                SPlayerPositionRotation, SPlayerRotation, SRenameItem, SSetCreativeSlot,
                SSetHeldItem, SSetPlayerGround, SSwingArm, SUpdateSign, SUseItem, SUseItemOn,
                SlotActionType, Status,
            },
        },
    },
    packet::MultiVersionJavaPacket,
    ser::NetworkReadExt,
};
use pumpkin_util::{
    GameMode, Hand,
    math::{position::BlockPos, vector3::Vector3},
    version::JavaMinecraftVersion,
};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{BufReader, BufWriter},
    net::{
        TcpStream,
        tcp::{OwnedReadHalf, OwnedWriteHalf},
    },
    time::timeout,
};
use uuid::Uuid;

use crate::bot::send_framed_packet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BotTarget {
    Primary,
    Observer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HandKind {
    MainHand,
    OffHand,
}

impl HandKind {
    #[must_use]
    pub const fn to_hand(self) -> Hand {
        match self {
            Self::MainHand => Hand::Left,
            Self::OffHand => Hand::Right,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlockFaceKind {
    Down,
    Up,
    North,
    South,
    West,
    East,
}

impl BlockFaceKind {
    #[must_use]
    pub const fn to_packet_index(self) -> i32 {
        match self {
            Self::Down => 0,
            Self::Up => 1,
            Self::North => 2,
            Self::South => 3,
            Self::West => 4,
            Self::East => 5,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlayerActionKind {
    StartedDigging,
    CancelledDigging,
    FinishedDigging,
    DropItemStack,
    DropItem,
    ReleaseUseItem,
    SwapItem,
}

impl PlayerActionKind {
    #[must_use]
    pub const fn to_status(self) -> Status {
        match self {
            Self::StartedDigging => Status::StartedDigging,
            Self::CancelledDigging => Status::CancelledDigging,
            Self::FinishedDigging => Status::FinishedDigging,
            Self::DropItemStack => Status::DropItemStack,
            Self::DropItem => Status::DropItem,
            Self::ReleaseUseItem => Status::ReleaseItemInUse,
            Self::SwapItem => Status::SwapItem,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlayerCommandKind {
    LeaveBed,
    StartSprinting,
    StopSprinting,
    StartHorseJump,
    StopHorseJump,
    OpenVehicleInventory,
    StartFallFlying,
}

impl PlayerCommandKind {
    #[must_use]
    pub const fn to_action(self) -> Action {
        match self {
            Self::LeaveBed => Action::LeaveBed,
            Self::StartSprinting => Action::StartSprinting,
            Self::StopSprinting => Action::StopSprinting,
            Self::StartHorseJump => Action::StartHorseJump,
            Self::StopHorseJump => Action::StopHorseJump,
            Self::OpenVehicleInventory => Action::OpenVehicleInventory,
            Self::StartFallFlying => Action::StartFlyingElytra,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClickSlotModeKind {
    Pickup,
    QuickMove,
    Swap,
    Clone,
    Throw,
    QuickCraft,
    PickupAll,
}

impl ClickSlotModeKind {
    #[must_use]
    pub const fn to_slot_action_type(self) -> SlotActionType {
        match self {
            Self::Pickup => SlotActionType::Pickup,
            Self::QuickMove => SlotActionType::QuickMove,
            Self::Swap => SlotActionType::Swap,
            Self::Clone => SlotActionType::Clone,
            Self::Throw => SlotActionType::Throw,
            Self::QuickCraft => SlotActionType::QuickCraft,
            Self::PickupAll => SlotActionType::PickupAll,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntityTargetKind {
    Observer,
    Primary,
    SpawnedIndex(usize),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action")]
pub enum LockstepAction {
    MovePos {
        offset: [f64; 3],
        on_ground: bool,
        horizontal_collision: bool,
    },
    MovePosRot {
        offset: [f64; 3],
        yaw: f32,
        pitch: f32,
        on_ground: bool,
        horizontal_collision: bool,
    },
    Rotate {
        yaw: f32,
        pitch: f32,
        on_ground: bool,
        horizontal_collision: bool,
    },
    SetGround {
        on_ground: bool,
        horizontal_collision: bool,
    },
    PlayerInput {
        flags: u8,
    },
    ClientTickEnd,
    PlayerCommand {
        command: PlayerCommandKind,
        jump_boost: i32,
    },
    PlayerAbilities {
        is_flying: bool,
    },
    SwingArm {
        hand: HandKind,
    },
    SetHeldItem {
        slot: i16,
    },
    ChangeGameMode {
        gamemode: u8,
    },
    ClientCommand {
        action_id: i32,
    },
    UseItemOn {
        hand: HandKind,
        offset: [i32; 3],
        face: BlockFaceKind,
        cursor: [f32; 3],
        inside_block: bool,
    },
    UseItem {
        hand: HandKind,
        yaw: f32,
        pitch: f32,
    },
    PlayerAction {
        status: PlayerActionKind,
        offset: [i32; 3],
        face: BlockFaceKind,
        use_sequence: bool,
    },
    PickItemFromBlock {
        offset: [i32; 3],
        include_data: bool,
    },
    UpdateSign {
        offset: [i32; 3],
        is_front_text: bool,
        lines: [String; 4],
    },
    SetCreativeSlot {
        slot: i16,
        item: Option<String>,
        count: u8,
    },
    ClickSlot {
        use_active_window: bool,
        window_id: u8,
        revision: i32,
        slot: i16,
        button: i8,
        mode: ClickSlotModeKind,
        changed_slots: Vec<(i16, Option<(String, u8)>)>,
        carried_item: Option<(String, u8)>,
    },
    ContainerButtonClick {
        use_active_window: bool,
        window_id: i32,
        button_id: i32,
    },
    ContainerSlotStateChanged {
        use_active_window: bool,
        window_id: i32,
        slot_id: i32,
        new_state: bool,
    },
    CloseContainer {
        use_active_window: bool,
        window_id: i8,
    },
    RenameItem {
        name: String,
    },
    PlaceRecipe {
        use_active_window: bool,
        window_id: i8,
        recipe_display_id: i32,
        use_max_items: bool,
    },
    AttackEntity {
        target: EntityTargetKind,
    },
    InteractEntity {
        target: EntityTargetKind,
        hand: HandKind,
        target_pos: Option<[f32; 3]>,
        sneak_key_pressed: bool,
    },
    ChatCommand {
        command: String,
    },
}

pub struct LockstepBot {
    pub reader: TCPNetworkDecoder<BufReader<OwnedReadHalf>>,
    pub writer: TCPNetworkEncoder<BufWriter<OwnedWriteHalf>>,
    pub version: JavaMinecraftVersion,
    pub name: String,
    pub entity_id: i32,
    pub pos: Vector3<f64>,
    pub yaw: f32,
    pub pitch: f32,
    pub sequence_counter: i32,
    pub active_window_id: Option<i32>,
    pub spawned_entity_ids: Vec<i32>,
    pub ping_nonce: i64,
    pub pending_auto_sent: usize,
}

impl LockstepBot {
    #[expect(
        clippy::too_many_lines,
        reason = "Performs complete Java login, configuration, and initial play handshake"
    )]
    pub async fn connect_and_login(
        addr: SocketAddr,
        name: &str,
        uuid: Uuid,
    ) -> Result<Self, String> {
        let stream = TcpStream::connect(addr)
            .await
            .map_err(|e| format!("Failed to connect lockstep bot {name} to {addr}: {e}"))?;
        let _ = stream.set_nodelay(true);
        let (read_half, write_half) = stream.into_split();
        let version = CURRENT_MC_VERSION;
        let mut bot = Self {
            reader: TCPNetworkDecoder::new(BufReader::new(read_half)),
            writer: TCPNetworkEncoder::new(BufWriter::new(write_half)),
            version,
            name: name.to_string(),
            entity_id: 0,
            pos: Vector3::new(0.0, 0.0, 0.0),
            yaw: 0.0,
            pitch: 0.0,
            sequence_counter: 0,
            active_window_id: None,
            spawned_entity_ids: Vec::new(),
            ping_nonce: 1000,
            pending_auto_sent: 0,
        };

        // 1. Handshake -> Login
        bot.send_packet(&SHandShake {
            protocol_version: VarInt(version.protocol_version()),
            server_address: "127.0.0.1".into(),
            server_port: addr.port(),
            next_state: ConnectionState::Login,
        })
        .await?;

        bot.send_packet(&SLoginStart {
            name: name.into(),
            uuid,
        })
        .await?;

        let expected_login_success = c_login::LOGIN_FINISHED.to_id(version);
        let login_compression_id = c_login::LOGIN_COMPRESSION.to_id(version);
        let login_disconnect_id = c_login::LOGIN_DISCONNECT.to_id(version);
        loop {
            let pkt = bot.read_raw_packet_timeout(Duration::from_secs(10)).await?;
            if pkt.id == login_compression_id {
                let mut slice = &pkt.payload[..];
                if let Ok(threshold) = slice.get_var_int()
                    && threshold.0 >= 0
                    && let Ok(thresh_usize) = usize::try_from(threshold.0)
                {
                    bot.reader.set_compression(thresh_usize);
                    bot.writer.set_compression((thresh_usize, 6));
                }
            } else if pkt.id == login_disconnect_id {
                return Err(format!(
                    "Bot '{name}' received LOGIN_DISCONNECT: {:?}",
                    String::from_utf8_lossy(&pkt.payload)
                ));
            } else if pkt.id == expected_login_success {
                break;
            }
        }

        bot.send_packet(&SLoginAcknowledged).await?;

        // 2. Configuration state
        bot.send_packet(&SClientInformationConfig {
            locale: "en_us",
            view_distance: 2,
            chat_mode: VarInt(0),
            chat_colors: true,
            skin_parts: 0x7f,
            main_hand: VarInt(1),
            text_filtering: false,
            server_listing: true,
            particle_status: VarInt(0),
        })
        .await?;

        let select_known_packs_id = c_config::SELECT_KNOWN_PACKS.to_id(version);
        let finish_config_id = c_config::FINISH_CONFIGURATION.to_id(version);
        let keep_alive_config_id = c_config::KEEP_ALIVE.to_id(version);
        let config_disconnect_id = c_config::DISCONNECT.to_id(version);
        let code_of_conduct_id = c_config::CODE_OF_CONDUCT.to_id(version);

        loop {
            let pkt = bot.read_raw_packet_timeout(Duration::from_secs(10)).await?;
            if pkt.id == config_disconnect_id {
                return Err(format!(
                    "Bot '{name}' received CONFIG_DISCONNECT: {:?}",
                    String::from_utf8_lossy(&pkt.payload)
                ));
            } else if pkt.id == select_known_packs_id {
                let mut slice = &pkt.payload[..];
                let packs = SKnownPacks::read(&mut slice, &version)
                    .map(|p| p.known_packs)
                    .unwrap_or_default();
                bot.send_packet(&SKnownPacks { known_packs: packs }).await?;
            } else if pkt.id == code_of_conduct_id {
                // Accept code of conduct if requested
                let accept_id = pumpkin_data::packet::serverbound::config::ACCEPT_CODE_OF_CONDUCT
                    .to_id(version);
                let mut buf = Vec::new();
                let _ = pumpkin_protocol::ser::NetworkWriteExt::write_var_int(
                    &mut buf,
                    &VarInt(accept_id),
                );
                let _ = bot.writer.write_packet(bytes::Bytes::from(buf)).await;
                let _ = bot.writer.flush().await;
            } else if pkt.id == keep_alive_config_id {
                let mut slice = &pkt.payload[..];
                if let Ok(ka) = CKeepAlive::read(&mut slice, &version) {
                    bot.send_packet(&SKeepAlive {
                        keep_alive_id: ka.keep_alive_id,
                    })
                    .await?;
                }
            } else if pkt.id == finish_config_id {
                bot.send_packet(&SAcknowledgeFinishConfig).await?;
                break;
            }
        }

        // 3. Play state login & initial position
        let play_login_id = c_play::LOGIN.to_id(version);
        let play_pos_id = c_play::PLAYER_POSITION.to_id(version);
        let chunk_batch_end_id = c_play::CHUNK_BATCH_FINISHED.to_id(version);
        let keep_alive_id = c_play::KEEP_ALIVE.to_id(version);

        let mut received_login = false;
        let mut received_pos = false;

        while !(received_login && received_pos) {
            let pkt = bot.read_raw_packet_timeout(Duration::from_secs(10)).await?;
            if pkt.id == play_login_id {
                let mut slice = &pkt.payload[..];
                bot.entity_id = slice
                    .get_i32_be()
                    .map_err(|e| format!("Failed to read entity_id from CLogin: {e:?}"))?;
                received_login = true;
            } else if pkt.id == play_pos_id {
                let mut slice = &pkt.payload[..];
                let pos = CPlayerPosition::read(&mut slice, &version)
                    .map_err(|e| format!("Failed to decode CPlayerPosition: {e:?}"))?;
                bot.pos = pos.position;
                bot.yaw = pos.yaw;
                bot.pitch = pos.pitch;
                bot.send_packet(&SConfirmTeleport {
                    teleport_id: pos.teleport_id,
                    position: pos.position,
                    yaw: pos.yaw,
                    pitch: pos.pitch,
                })
                .await?;
                bot.send_packet(&SPlayerLoaded).await?;
                received_pos = true;
            } else if pkt.id == chunk_batch_end_id {
                bot.send_packet(&SChunkBatch {
                    chunks_per_tick: 25.0,
                })
                .await?;
            } else if pkt.id == keep_alive_id {
                let mut slice = &pkt.payload[..];
                if let Ok(ka) = CKeepAlive::read(&mut slice, &version) {
                    bot.send_packet(&SKeepAlive {
                        keep_alive_id: ka.keep_alive_id,
                    })
                    .await?;
                }
            }
        }

        Ok(bot)
    }

    pub const fn next_sequence(&mut self) -> i32 {
        self.sequence_counter += 1;
        self.sequence_counter
    }

    pub async fn send_packet<P: ClientPacket + MultiVersionJavaPacket>(
        &mut self,
        packet: &P,
    ) -> Result<(), String> {
        send_framed_packet(&mut self.writer, packet).await
    }

    pub async fn read_raw_packet_timeout(&mut self, dur: Duration) -> Result<RawPacket, String> {
        match timeout(dur, self.reader.get_raw_packet()).await {
            Ok(Ok(pkt)) => Ok(pkt),
            Ok(Err(e)) => Err(format!("Bot '{}' packet decode error: {e}", self.name)),
            Err(_) => Err(format!(
                "Timed out after {dur:?} waiting for packet on {}",
                self.name
            )),
        }
    }

    /// Sends a play ping request with a fresh nonce and reads all packets up to the matching
    /// `PONG_RESPONSE`. Automatically handles `KEEP_ALIVE`, `PLAYER_POSITION` teleport acks,
    /// and `CHUNK_BATCH_FINISHED` while returning all raw `(packet_id, payload)` pairs.
    pub async fn drain_until_pong(
        &mut self,
        wait_timeout: Duration,
    ) -> Result<Vec<(i32, Vec<u8>)>, String> {
        self.ping_nonce += 1;
        let expected_nonce = self.ping_nonce;
        self.send_packet(&SPlayPingRequest {
            payload: expected_nonce,
        })
        .await?;

        self.read_until_pong_nonce(expected_nonce, wait_timeout)
            .await
    }

    #[allow(clippy::too_many_lines)]
    pub async fn read_until_pong_nonce(
        &mut self,
        expected_nonce: i64,
        wait_timeout: Duration,
    ) -> Result<Vec<(i32, Vec<u8>)>, String> {
        let v = self.version;
        let pong_id = c_play::PONG_RESPONSE.to_id(v);
        let disconnect_id = c_play::DISCONNECT.to_id(v);
        let keep_alive_id = c_play::KEEP_ALIVE.to_id(v);
        let player_pos_id = c_play::PLAYER_POSITION.to_id(v);
        let chunk_batch_end_id = c_play::CHUNK_BATCH_FINISHED.to_id(v);
        let open_screen_id = c_play::OPEN_SCREEN.to_id(v);
        let close_container_id = c_play::CONTAINER_CLOSE.to_id(v);
        let add_entity_id = c_play::ADD_ENTITY.to_id(v);

        let mut collected = Vec::new();
        loop {
            let pkt = self.read_raw_packet_timeout(wait_timeout).await?;
            let id = pkt.id;
            let data = pkt.payload.to_vec();
            if id == disconnect_id {
                return Err(format!(
                    "Bot '{}' received DISCONNECT packet from server: {:?}",
                    self.name,
                    String::from_utf8_lossy(&data)
                ));
            }
            if id == pong_id {
                let mut slice = data.as_slice();
                if let Ok(nonce) = slice.get_i64_be()
                    && nonce == expected_nonce
                {
                    break;
                }
                continue;
            }
            if id == keep_alive_id {
                let mut slice = data.as_slice();
                if let Ok(ka) = CKeepAlive::read(&mut slice, &v) {
                    self.send_packet(&SKeepAlive {
                        keep_alive_id: ka.keep_alive_id,
                    })
                    .await?;
                    self.pending_auto_sent += 1;
                }
                continue;
            }
            if id == chunk_batch_end_id {
                self.send_packet(&SChunkBatch {
                    chunks_per_tick: 25.0,
                })
                .await?;
                self.pending_auto_sent += 1;
                continue;
            }
            if id == player_pos_id {
                let mut slice = data.as_slice();
                if let Ok(pos) = CPlayerPosition::read(&mut slice, &v) {
                    if pos.relatives.contains(&PositionFlag::X) {
                        self.pos.x += pos.position.x;
                    } else {
                        self.pos.x = pos.position.x;
                    }
                    if pos.relatives.contains(&PositionFlag::Y) {
                        self.pos.y += pos.position.y;
                    } else {
                        self.pos.y = pos.position.y;
                    }
                    if pos.relatives.contains(&PositionFlag::Z) {
                        self.pos.z += pos.position.z;
                    } else {
                        self.pos.z = pos.position.z;
                    }
                    if pos.relatives.contains(&PositionFlag::YRot) {
                        self.yaw += pos.yaw;
                    } else {
                        self.yaw = pos.yaw;
                    }
                    if pos.relatives.contains(&PositionFlag::XRot) {
                        self.pitch += pos.pitch;
                    } else {
                        self.pitch = pos.pitch;
                    }
                    self.send_packet(&SConfirmTeleport {
                        teleport_id: pos.teleport_id,
                        position: self.pos,
                        yaw: self.yaw,
                        pitch: self.pitch,
                    })
                    .await?;
                    self.pending_auto_sent += 1;
                }
            } else if id == open_screen_id {
                let mut slice = data.as_slice();
                if let Ok(win_id) = slice.get_var_int() {
                    self.active_window_id = Some(win_id.0);
                }
            } else if id == close_container_id {
                self.active_window_id = None;
            } else if id == add_entity_id {
                let mut slice = data.as_slice();
                if let Ok(spawn) = CSpawnEntity::read_packet_data(&mut slice, &v) {
                    self.spawned_entity_ids.push(spawn.entity_id.0);
                } else {
                    let mut fallback = data.as_slice();
                    if let Ok(eid) = fallback.get_var_int() {
                        self.spawned_entity_ids.push(eid.0);
                    }
                }
            }
            collected.push((id, data));
        }
        Ok(collected)
    }

    #[allow(clippy::too_many_lines)]
    pub async fn execute_action(
        &mut self,
        action: &LockstepAction,
        origin: BlockPos,
        peer_entity_id: Option<i32>,
    ) -> Result<(), String> {
        match action {
            LockstepAction::MovePos {
                offset,
                on_ground,
                horizontal_collision,
            } => {
                let pos = Vector3::new(
                    f64::from(origin.0.x) + offset[0],
                    f64::from(origin.0.y) + offset[1],
                    f64::from(origin.0.z) + offset[2],
                );
                self.pos = pos;
                let collision = u8::from(*on_ground) | (u8::from(*horizontal_collision) << 1);
                self.send_packet(&SPlayerPosition {
                    position: pos,
                    collision,
                })
                .await
            }
            LockstepAction::MovePosRot {
                offset,
                yaw,
                pitch,
                on_ground,
                horizontal_collision,
            } => {
                let pos = Vector3::new(
                    f64::from(origin.0.x) + offset[0],
                    f64::from(origin.0.y) + offset[1],
                    f64::from(origin.0.z) + offset[2],
                );
                self.pos = pos;
                self.yaw = *yaw;
                self.pitch = *pitch;
                let collision = u8::from(*on_ground) | (u8::from(*horizontal_collision) << 1);
                self.send_packet(&SPlayerPositionRotation {
                    position: pos,
                    yaw: *yaw,
                    pitch: *pitch,
                    collision,
                })
                .await
            }
            LockstepAction::Rotate {
                yaw,
                pitch,
                on_ground,
                ..
            } => {
                self.yaw = *yaw;
                self.pitch = *pitch;
                self.send_packet(&SPlayerRotation {
                    yaw: *yaw,
                    pitch: *pitch,
                    ground: *on_ground,
                })
                .await
            }
            LockstepAction::SetGround { on_ground, .. } => {
                self.send_packet(&SSetPlayerGround {
                    on_ground: *on_ground,
                })
                .await
            }
            LockstepAction::PlayerInput { flags } => {
                self.send_packet(&SPlayerInput {
                    input: *flags as i8,
                })
                .await
            }
            LockstepAction::ClientTickEnd => self.send_packet(&SClientTickEnd).await,
            LockstepAction::PlayerCommand {
                command,
                jump_boost,
            } => {
                self.send_packet(&SPlayerCommand {
                    entity_id: VarInt(self.entity_id),
                    action: command.to_action(),
                    jump_boost: VarInt(*jump_boost),
                })
                .await
            }
            LockstepAction::PlayerAbilities { is_flying } => {
                self.send_packet(&SPlayerAbilities {
                    flags: if *is_flying { 0x02 } else { 0x00 },
                    fly_speed: None,
                    walk_speed: None,
                })
                .await
            }
            LockstepAction::SwingArm { hand } => {
                self.send_packet(&SSwingArm {
                    hand: VarInt(hand.to_hand() as i32),
                })
                .await
            }
            LockstepAction::SetHeldItem { slot } => {
                self.send_packet(&SSetHeldItem { slot: *slot }).await
            }
            LockstepAction::ChangeGameMode { gamemode } => {
                let mode = GameMode::try_from(*gamemode as i8).unwrap_or(GameMode::Survival);
                self.send_packet(&SChangeGameMode { game_mode: mode }).await
            }
            LockstepAction::ClientCommand { action_id } => {
                self.send_packet(&SClientCommand::new(VarInt(*action_id)))
                    .await
            }
            LockstepAction::UseItemOn {
                hand,
                offset,
                face,
                cursor,
                inside_block,
            } => {
                let seq = self.next_sequence();
                let pos = BlockPos::new(
                    origin.0.x + offset[0],
                    origin.0.y + offset[1],
                    origin.0.z + offset[2],
                );
                self.send_packet(&SUseItemOn {
                    hand: VarInt(hand.to_hand() as i32),
                    position: pos,
                    face: VarInt(face.to_packet_index()),
                    cursor_pos: Vector3::new(cursor[0], cursor[1], cursor[2]),
                    inside_block: *inside_block,
                    is_against_world_border: false,
                    sequence: VarInt(seq),
                })
                .await
            }
            LockstepAction::UseItem { hand, yaw, pitch } => {
                let seq = self.next_sequence();
                self.send_packet(&SUseItem {
                    hand: VarInt(hand.to_hand() as i32),
                    sequence: VarInt(seq),
                    yaw: *yaw,
                    pitch: *pitch,
                })
                .await
            }
            LockstepAction::PlayerAction {
                status,
                offset,
                face,
                use_sequence,
            } => {
                let seq = if *use_sequence {
                    self.next_sequence()
                } else {
                    0
                };
                let pos = BlockPos::new(
                    origin.0.x + offset[0],
                    origin.0.y + offset[1],
                    origin.0.z + offset[2],
                );
                self.send_packet(&SPlayerAction {
                    status: VarInt(status.to_status() as i32),
                    position: pos,
                    face: face.to_packet_index() as u8,
                    sequence: VarInt(seq),
                })
                .await
            }
            LockstepAction::PickItemFromBlock {
                offset,
                include_data,
            } => {
                let pos = BlockPos::new(
                    origin.0.x + offset[0],
                    origin.0.y + offset[1],
                    origin.0.z + offset[2],
                );
                self.send_packet(&SPickItemFromBlock {
                    pos,
                    include_data: *include_data,
                })
                .await
            }
            LockstepAction::UpdateSign {
                offset,
                is_front_text,
                lines,
            } => {
                let pos = BlockPos::new(
                    origin.0.x + offset[0],
                    origin.0.y + offset[1],
                    origin.0.z + offset[2],
                );
                self.send_packet(&SUpdateSign {
                    location: pos,
                    is_front_text: *is_front_text,
                    line_1: &lines[0],
                    line_2: &lines[1],
                    line_3: &lines[2],
                    line_4: &lines[3],
                })
                .await
            }
            LockstepAction::SetCreativeSlot { slot, item, count } => {
                let clicked_item = build_item_serializer(item.as_deref(), *count)?;
                self.send_packet(&SSetCreativeSlot {
                    slot: *slot,
                    clicked_item,
                })
                .await
            }
            LockstepAction::ClickSlot {
                use_active_window,
                window_id,
                revision,
                slot,
                button,
                mode,
                changed_slots,
                carried_item,
            } => {
                let sync_id = if *use_active_window {
                    self.active_window_id.unwrap_or(i32::from(*window_id))
                } else {
                    i32::from(*window_id)
                };
                let array_of_changed_slots: Vec<(i16, OptionalItemStackHash)> = changed_slots
                    .iter()
                    .map(|(s, item_opt)| (*s, make_optional_item_hash(item_opt.as_ref())))
                    .collect();
                let length_of_array =
                    VarInt(i32::try_from(array_of_changed_slots.len()).unwrap_or(0));
                let carried = make_optional_item_hash(carried_item.as_ref());
                self.send_packet(&SClickSlot {
                    sync_id: VarInt(sync_id),
                    revision: VarInt(*revision),
                    slot: *slot,
                    button: *button,
                    mode: mode.to_slot_action_type(),
                    length_of_array,
                    array_of_changed_slots,
                    carried_item: carried,
                })
                .await
            }
            LockstepAction::ContainerButtonClick {
                use_active_window,
                window_id,
                button_id,
            } => {
                let win = if *use_active_window {
                    self.active_window_id.unwrap_or(*window_id)
                } else {
                    *window_id
                };
                self.send_packet(&SContainerButtonClick {
                    window_id: VarInt(win),
                    button_id: VarInt(*button_id),
                })
                .await
            }
            LockstepAction::ContainerSlotStateChanged {
                use_active_window,
                window_id,
                slot_id,
                new_state,
            } => {
                let win = if *use_active_window {
                    self.active_window_id.unwrap_or(*window_id)
                } else {
                    *window_id
                };
                self.send_packet(&SContainerSlotStateChanged {
                    slot_id: VarInt(*slot_id),
                    container_id: VarInt(win),
                    new_state: *new_state,
                })
                .await
            }
            LockstepAction::CloseContainer {
                use_active_window,
                window_id,
            } => {
                let win = if *use_active_window {
                    self.active_window_id.unwrap_or(i32::from(*window_id))
                } else {
                    i32::from(*window_id)
                };
                self.active_window_id = None;
                self.send_packet(&SCloseContainer {
                    window_id: VarInt(win),
                })
                .await
            }
            LockstepAction::RenameItem { name } => {
                self.send_packet(&SRenameItem { item_name: name }).await
            }
            LockstepAction::PlaceRecipe {
                use_active_window,
                window_id,
                recipe_display_id,
                use_max_items,
            } => {
                let win = if *use_active_window {
                    i8::try_from(self.active_window_id.unwrap_or(i32::from(*window_id)))
                        .unwrap_or(*window_id)
                } else {
                    *window_id
                };
                self.send_packet(&SPlaceRecipe {
                    container_id: win,
                    recipe_display_id: VarInt(*recipe_display_id),
                    use_max_items: *use_max_items,
                })
                .await
            }
            LockstepAction::AttackEntity { target } => {
                let entity_id = self.resolve_entity_target(*target, peer_entity_id)?;
                self.send_packet(&SAttack {
                    entity_id: VarInt(entity_id),
                })
                .await
            }
            LockstepAction::InteractEntity {
                target,
                hand,
                target_pos,
                sneak_key_pressed,
            } => {
                let entity_id = self.resolve_entity_target(*target, peer_entity_id)?;
                let action_type = if target_pos.is_some() {
                    ActionType::InteractAt
                } else {
                    ActionType::Interact
                };
                self.send_packet(&SInteract {
                    entity_id: VarInt(entity_id),
                    r#type: VarInt(action_type as i32),
                    target_position: target_pos
                        .map(|p| Vector3::new(f64::from(p[0]), f64::from(p[1]), f64::from(p[2]))),
                    hand: Some(VarInt(hand.to_hand() as i32)),
                    sneaking: *sneak_key_pressed,
                })
                .await
            }
            LockstepAction::ChatCommand { command } => {
                self.send_packet(&SChatCommand { command }).await
            }
        }
    }

    fn resolve_entity_target(
        &self,
        target: EntityTargetKind,
        peer_entity_id: Option<i32>,
    ) -> Result<i32, String> {
        match target {
            EntityTargetKind::Primary => Ok(self.entity_id),
            EntityTargetKind::Observer => {
                peer_entity_id.ok_or_else(|| "No observer entity_id available".to_string())
            }
            EntityTargetKind::SpawnedIndex(idx) => self
                .spawned_entity_ids
                .get(idx)
                .copied()
                .ok_or_else(|| format!("No spawned entity at index {idx}")),
        }
    }
}

fn build_item_serializer(
    item_key: Option<&str>,
    count: u8,
) -> Result<ItemStackSerializer<'static>, String> {
    let Some(key) = item_key else {
        return Ok(ItemStackSerializer(Cow::Borrowed(ItemStack::EMPTY)));
    };
    if count == 0 {
        return Ok(ItemStackSerializer(Cow::Borrowed(ItemStack::EMPTY)));
    }
    let stripped = key.strip_prefix("minecraft:").unwrap_or(key);
    let item = Item::from_registry_key(stripped)
        .ok_or_else(|| format!("Unknown item registry key '{key}'"))?;
    Ok(ItemStackSerializer(Cow::Owned(ItemStack::new(count, item))))
}

fn make_optional_item_hash(item_opt: Option<&(String, u8)>) -> OptionalItemStackHash {
    let Some((key, count)) = item_opt else {
        return OptionalItemStackHash(None);
    };
    if *count == 0 {
        return OptionalItemStackHash(None);
    }
    let stripped = key.strip_prefix("minecraft:").unwrap_or(key.as_str());
    let Some(item) = Item::from_registry_key(stripped) else {
        return OptionalItemStackHash(None);
    };
    OptionalItemStackHash(Some(ItemStackHash {
        item_id: VarInt(i32::from(item.id)),
        count: VarInt(i32::from(*count)),
        components: ItemComponentHash {
            added: Vec::new(),
            removed: Vec::new(),
        },
    }))
}
