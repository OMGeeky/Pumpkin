use std::collections::BTreeMap;

use pumpkin_data::{
    BlockStateId,
    entity::EntityType,
    item::Item,
    packet::{CURRENT_MC_VERSION, clientbound::play as c_play},
};
use pumpkin_gametest::format_canonical_block_state;
use pumpkin_protocol::{
    IdOr, PositionFlag, ServerPacket, SoundEvent, VarInt,
    codec::{item_stack_seralizer::ItemStackSerializer, lp_vector_3d::LpVector3d},
    java::client::play::{
        CBlockEntityData, CCloseContainer, CHeadRot, CMultiBlockUpdate, COpenSignEditor,
        CPlayerPosition, CRespawn, CSetContainerContent, CSetContainerProperty, CSetContainerSlot,
        CSetEntityMetadata, CSetEquipment, CSetExperience, CSetHealth, CSpawnEntity,
        CTakeItemEntity, CUpdateAttributes, CUpdateEntityPos, CUpdateEntityPosRot,
        CUpdateEntityRot,
    },
    ser::NetworkReadExt,
};
use pumpkin_util::math::{position::BlockPos, vector3::Vector3};
use serde::{Deserialize, Serialize};

#[must_use]
pub fn round3(v: f64) -> f64 {
    let r = (v * 1000.0).round() / 1000.0;
    if r == 0.0 { 0.0 } else { r }
}

#[must_use]
pub fn round3_f32(v: f32) -> f32 {
    let r = (v * 1000.0).round() / 1000.0;
    if r == 0.0 { 0.0 } else { r }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RecordedBlockUpdate {
    pub pos: [i32; 3],
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RecordedItemStack {
    pub item: String,
    pub count: u8,
}

impl RecordedItemStack {
    #[must_use]
    pub fn from_serializer(slot: &ItemStackSerializer<'_>) -> Option<Self> {
        let stack = slot.0.as_ref();
        if stack.is_empty() {
            return None;
        }
        Some(Self {
            item: format!("minecraft:{}", stack.item.registry_key),
            count: stack.item_count,
        })
    }

    #[must_use]
    pub fn from_id_and_count(item_id: u16, count: u8) -> Option<Self> {
        if count == 0 || item_id == 0 {
            return None;
        }
        let item_name = Item::from_id(item_id).map_or_else(
            || format!("unknown_item_{item_id}"),
            |it| format!("minecraft:{}", it.registry_key),
        );
        Some(Self {
            item: item_name,
            count,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RecordedSlotItem {
    pub slot: usize,
    pub item: RecordedItemStack,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RecordedEquipmentSlot {
    pub slot: i8,
    pub item: Option<RecordedItemStack>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordedAttribute {
    pub id: i32,
    pub base_value: f64,
    pub modifier_count: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "packet")]
pub enum RecordedClientPacket {
    BlockUpdate {
        pos: [i32; 3],
        state: String,
    },
    SectionBlocksUpdate {
        updates: Vec<RecordedBlockUpdate>,
    },
    BlockChangedAck {
        sequence: i32,
    },
    BlockEvent {
        pos: [i32; 3],
        action_id: u8,
        action_param: u8,
        block: String,
    },
    BlockEntityData {
        pos: [i32; 3],
        block_entity_type: i32,
        has_nbt: bool,
    },
    BlockDestruction {
        target: String,
        pos: [i32; 3],
        stage: i8,
    },
    LevelEvent {
        event: i32,
        pos: [i32; 3],
        data: i32,
        disable_relative_volume: bool,
    },
    OpenSignEditor {
        pos: [i32; 3],
        is_front_text: bool,
    },
    OpenScreen {
        window_id: i32,
        window_type: i32,
        title: String,
    },
    CloseContainer {
        window_id: i32,
    },
    ContainerSetContent {
        window_id: i32,
        non_empty_slots: Vec<RecordedSlotItem>,
        carried: Option<RecordedItemStack>,
    },
    ContainerSetSlot {
        window_id: i32,
        slot: i16,
        item: Option<RecordedItemStack>,
    },
    ContainerSetData {
        window_id: i32,
        property: i16,
        value: i16,
    },
    SetCursorItem {
        item: Option<RecordedItemStack>,
    },
    SetPlayerInventory {
        slot: i32,
        item: Option<RecordedItemStack>,
    },
    SetHeldSlot {
        slot: i8,
    },
    ItemCooldown {
        group: String,
        ticks: i32,
    },
    SpawnEntity {
        target: String,
        entity_type: String,
        pos: [f64; 3],
        yaw: u8,
        pitch: u8,
        head_yaw: u8,
        data: i32,
        velocity: [f64; 3],
    },
    RemoveEntities {
        targets: Vec<String>,
    },
    SetEntityData {
        target: String,
        shared_flags: Option<u8>,
        raw_hex: String,
    },
    MoveEntityPos {
        target: String,
        delta: [i16; 3],
        on_ground: bool,
    },
    MoveEntityPosRot {
        target: String,
        delta: [i16; 3],
        yaw: u8,
        pitch: u8,
        on_ground: bool,
    },
    MoveEntityRot {
        target: String,
        yaw: u8,
        pitch: u8,
        on_ground: bool,
    },
    RotateHead {
        target: String,
        head_yaw: u8,
    },
    EntityPositionSync {
        target: String,
        pos: [f64; 3],
        yaw: f32,
        pitch: f32,
        on_ground: bool,
    },
    SetEntityMotion {
        target: String,
        velocity: [f64; 3],
    },
    SetEquipment {
        target: String,
        slots: Vec<RecordedEquipmentSlot>,
    },
    EntityEvent {
        target: String,
        status: i8,
    },
    Animate {
        target: String,
        animation: String,
    },
    HurtAnimation {
        target: String,
        yaw: f32,
    },
    DamageEvent {
        target: String,
        source_type_id: i32,
        has_cause: bool,
        has_direct: bool,
    },
    TakeItemEntity {
        item_target: String,
        collector_target: String,
        amount: i32,
    },
    SetHealth {
        health: f32,
        food: i32,
        saturation: f32,
    },
    SetExperience {
        progress: f32,
        level: i32,
        total: i32,
    },
    UpdateAttributes {
        target: String,
        attributes: Vec<RecordedAttribute>,
    },
    UpdateMobEffect {
        target: String,
        effect_id: i32,
        amplifier: i32,
        duration: i32,
        flags: i8,
    },
    RemoveMobEffect {
        target: String,
        effect_id: i32,
    },
    PlayerAbilities {
        flags: i8,
        flying_speed: f32,
        field_of_view: f32,
    },
    GameEvent {
        event: u8,
        value: f32,
    },
    PlayerPosition {
        pos: [f64; 3],
        delta: [f64; 3],
        yaw: f32,
        pitch: f32,
        relatives_count: usize,
    },
    PlayerRotation {
        yaw: f32,
        is_yaw_relative: bool,
        pitch: f32,
        is_pitch_relative: bool,
    },
    Respawn {
        gamemode: u8,
        dimension: String,
    },
    TickingState {
        tick_rate: f32,
        is_frozen: bool,
    },
    TickingStep {
        tick_steps: i32,
    },
    Sound {
        sound: String,
        category: i32,
        pos: [i32; 3],
    },
    UnknownGameplayPacket {
        packet_id: i32,
        payload_len: usize,
    },
}

impl RecordedClientPacket {
    #[must_use]
    pub const fn packet_kind(&self) -> &'static str {
        match self {
            Self::BlockUpdate { .. } => "BlockUpdate",
            Self::SectionBlocksUpdate { .. } => "SectionBlocksUpdate",
            Self::BlockChangedAck { .. } => "BlockChangedAck",
            Self::BlockEvent { .. } => "BlockEvent",
            Self::BlockEntityData { .. } => "BlockEntityData",
            Self::BlockDestruction { .. } => "BlockDestruction",
            Self::LevelEvent { .. } => "LevelEvent",
            Self::OpenSignEditor { .. } => "OpenSignEditor",
            Self::OpenScreen { .. } => "OpenScreen",
            Self::CloseContainer { .. } => "CloseContainer",
            Self::ContainerSetContent { .. } => "ContainerSetContent",
            Self::ContainerSetSlot { .. } => "ContainerSetSlot",
            Self::ContainerSetData { .. } => "ContainerSetData",
            Self::SetCursorItem { .. } => "SetCursorItem",
            Self::SetPlayerInventory { .. } => "SetPlayerInventory",
            Self::SetHeldSlot { .. } => "SetHeldSlot",
            Self::ItemCooldown { .. } => "ItemCooldown",
            Self::SpawnEntity { .. } => "SpawnEntity",
            Self::RemoveEntities { .. } => "RemoveEntities",
            Self::SetEntityData { .. } => "SetEntityData",
            Self::MoveEntityPos { .. } => "MoveEntityPos",
            Self::MoveEntityPosRot { .. } => "MoveEntityPosRot",
            Self::MoveEntityRot { .. } => "MoveEntityRot",
            Self::RotateHead { .. } => "RotateHead",
            Self::EntityPositionSync { .. } => "EntityPositionSync",
            Self::SetEntityMotion { .. } => "SetEntityMotion",
            Self::SetEquipment { .. } => "SetEquipment",
            Self::EntityEvent { .. } => "EntityEvent",
            Self::Animate { .. } => "Animate",
            Self::HurtAnimation { .. } => "HurtAnimation",
            Self::DamageEvent { .. } => "DamageEvent",
            Self::TakeItemEntity { .. } => "TakeItemEntity",
            Self::SetHealth { .. } => "SetHealth",
            Self::SetExperience { .. } => "SetExperience",
            Self::UpdateAttributes { .. } => "UpdateAttributes",
            Self::UpdateMobEffect { .. } => "UpdateMobEffect",
            Self::RemoveMobEffect { .. } => "RemoveMobEffect",
            Self::PlayerAbilities { .. } => "PlayerAbilities",
            Self::GameEvent { .. } => "GameEvent",
            Self::PlayerPosition { .. } => "PlayerPosition",
            Self::PlayerRotation { .. } => "PlayerRotation",
            Self::Respawn { .. } => "Respawn",
            Self::TickingState { .. } => "TickingState",
            Self::TickingStep { .. } => "TickingStep",
            Self::Sound { .. } => "Sound",
            Self::UnknownGameplayPacket { .. } => "UnknownGameplayPacket",
        }
    }
}

/// Checks whether a clientbound Play packet ID is transport-only or background noise
/// that must be excluded from deterministic lockstep comparison.
#[must_use]
pub const fn is_transport_only_packet(packet_id: i32, include_sounds: bool) -> bool {
    let v = CURRENT_MC_VERSION;
    if packet_id == c_play::BUNDLE_DELIMITER.to_id(v)
        || packet_id == c_play::KEEP_ALIVE.to_id(v)
        || packet_id == c_play::PING.to_id(v)
        || packet_id == c_play::PONG_RESPONSE.to_id(v)
        || packet_id == c_play::CHUNK_BATCH_START.to_id(v)
        || packet_id == c_play::CHUNK_BATCH_FINISHED.to_id(v)
        || packet_id == c_play::LEVEL_CHUNK_WITH_LIGHT.to_id(v)
        || packet_id == c_play::LIGHT_UPDATE.to_id(v)
        || packet_id == c_play::FORGET_LEVEL_CHUNK.to_id(v)
        || packet_id == c_play::CHUNKS_BIOMES.to_id(v)
        || packet_id == c_play::SET_CHUNK_CACHE_CENTER.to_id(v)
        || packet_id == c_play::SET_CHUNK_CACHE_RADIUS.to_id(v)
        || packet_id == c_play::SET_SIMULATION_DISTANCE.to_id(v)
        || packet_id == c_play::SET_TIME.to_id(v)
        || packet_id == c_play::PLAYER_INFO_UPDATE.to_id(v)
        || packet_id == c_play::PLAYER_INFO_REMOVE.to_id(v)
        || packet_id == c_play::TAB_LIST.to_id(v)
        || packet_id == c_play::COMMANDS.to_id(v)
        || packet_id == c_play::UPDATE_RECIPES.to_id(v)
        || packet_id == c_play::RECIPE_BOOK_ADD.to_id(v)
        || packet_id == c_play::RECIPE_BOOK_REMOVE.to_id(v)
        || packet_id == c_play::RECIPE_BOOK_SETTINGS.to_id(v)
        || packet_id == c_play::UPDATE_TAGS.to_id(v)
        || packet_id == c_play::SERVER_DATA.to_id(v)
        || packet_id == c_play::SERVER_LINKS.to_id(v)
        || packet_id == c_play::INITIALIZE_BORDER.to_id(v)
        || packet_id == c_play::SET_BORDER_CENTER.to_id(v)
        || packet_id == c_play::SET_BORDER_LERP_SIZE.to_id(v)
        || packet_id == c_play::SET_BORDER_SIZE.to_id(v)
        || packet_id == c_play::SET_BORDER_WARNING_DELAY.to_id(v)
        || packet_id == c_play::SET_BORDER_WARNING_DISTANCE.to_id(v)
        || packet_id == c_play::SYSTEM_CHAT.to_id(v)
        || packet_id == c_play::PLAYER_CHAT.to_id(v)
        || packet_id == c_play::DISGUISED_CHAT.to_id(v)
        || packet_id == c_play::DELETE_CHAT.to_id(v)
        || packet_id == c_play::CUSTOM_CHAT_COMPLETIONS.to_id(v)
        || packet_id == c_play::CHANGE_DIFFICULTY.to_id(v)
        || packet_id == c_play::SET_DEFAULT_SPAWN_POSITION.to_id(v)
        || packet_id == c_play::UPDATE_ADVANCEMENTS.to_id(v)
        || packet_id == c_play::AWARD_STATS.to_id(v)
        || packet_id == c_play::CUSTOM_PAYLOAD.to_id(v)
        || packet_id == c_play::LEVEL_PARTICLES.to_id(v)
        || packet_id == c_play::SOUND_ENTITY.to_id(v)
        || packet_id == c_play::STOP_SOUND.to_id(v)
        || packet_id == c_play::GAME_RULE_VALUES.to_id(v)
        || packet_id == c_play::WAYPOINT.to_id(v)
    {
        return true;
    }
    if !include_sounds && packet_id == c_play::SOUND.to_id(v) {
        return true;
    }
    false
}

/// Normalizes dynamic IDs (entity IDs, container window IDs) and relative coordinates
/// across lockstep server runs so packet streams can be compared deterministically.
#[derive(Debug, Clone)]
pub struct PacketNormalizer {
    pub origin: BlockPos,
    pub self_entity_id: i32,
    pub observer_entity_id: Option<i32>,
    pub entity_map: BTreeMap<i32, String>,
    pub window_map: BTreeMap<i32, i32>,
    pub include_sounds: bool,
}

impl PacketNormalizer {
    #[must_use]
    pub const fn new(
        origin: BlockPos,
        self_entity_id: i32,
        observer_entity_id: Option<i32>,
        include_sounds: bool,
    ) -> Self {
        Self {
            origin,
            self_entity_id,
            observer_entity_id,
            entity_map: BTreeMap::new(),
            window_map: BTreeMap::new(),
            include_sounds,
        }
    }

    pub fn register_entity_label(&mut self, raw_entity_id: i32, label: impl Into<String>) {
        self.entity_map.insert(raw_entity_id, label.into());
    }

    #[must_use]
    pub fn lookup_entity_id(&self, raw_entity_id: i32) -> Option<String> {
        if raw_entity_id == self.self_entity_id {
            return Some("self".to_string());
        }
        if self.observer_entity_id == Some(raw_entity_id) {
            return Some("observer".to_string());
        }
        self.entity_map.get(&raw_entity_id).cloned()
    }

    pub fn normalize_entity_id(&mut self, raw_entity_id: i32) -> String {
        if let Some(known) = self.lookup_entity_id(raw_entity_id) {
            return known;
        }
        let next_idx = self.entity_map.len();
        let label = format!("entity_{next_idx}");
        self.entity_map.insert(raw_entity_id, label.clone());
        label
    }

    pub fn normalize_window_id(&mut self, raw_window_id: i32) -> i32 {
        if raw_window_id <= 0 {
            return raw_window_id;
        }
        if let Some(&mapped) = self.window_map.get(&raw_window_id) {
            return mapped;
        }
        let next = i32::try_from(self.window_map.len()).unwrap_or(0) + 1;
        self.window_map.insert(raw_window_id, next);
        next
    }

    #[must_use]
    pub const fn rel_block_pos(&self, pos: BlockPos) -> [i32; 3] {
        [
            pos.0.x - self.origin.0.x,
            pos.0.y - self.origin.0.y,
            pos.0.z - self.origin.0.z,
        ]
    }

    #[must_use]
    pub fn rel_f64_pos(&self, pos: Vector3<f64>) -> [f64; 3] {
        [
            round3(pos.x - f64::from(self.origin.0.x)),
            round3(pos.y - f64::from(self.origin.0.y)),
            round3(pos.z - f64::from(self.origin.0.z)),
        ]
    }

    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn decode_packet(
        &mut self,
        packet_id: i32,
        payload: &[u8],
    ) -> Option<RecordedClientPacket> {
        let v = CURRENT_MC_VERSION;
        if is_transport_only_packet(packet_id, self.include_sounds) {
            return None;
        }
        let mut buf = payload;

        if packet_id == c_play::BLOCK_UPDATE.to_id(v) {
            let location = buf.get_block_pos(&v).ok()?;
            let pos = self.rel_block_pos(location);
            if pos[0].abs() > 16 || pos[1].abs() > 16 || pos[2].abs() > 16 {
                return None;
            }
            let raw_state_id = u16::try_from(buf.get_var_int().ok()?.0).unwrap_or(0);
            let state_id = BlockStateId::new_or_air(raw_state_id);
            return Some(RecordedClientPacket::BlockUpdate {
                pos,
                state: format_canonical_block_state(state_id),
            });
        }

        if packet_id == c_play::SECTION_BLOCKS_UPDATE.to_id(v) {
            let pkt = CMultiBlockUpdate::read(&mut buf, &v).ok()?;
            let mut updates: Vec<RecordedBlockUpdate> = pkt
                .updates
                .into_iter()
                .map(|(pos, state_id)| RecordedBlockUpdate {
                    pos: self.rel_block_pos(pos),
                    state: format_canonical_block_state(state_id),
                })
                .filter(|u| u.pos[0].abs() <= 16 && u.pos[1].abs() <= 16 && u.pos[2].abs() <= 16)
                .collect();
            if updates.is_empty() {
                return None;
            }
            updates.sort();
            return Some(RecordedClientPacket::SectionBlocksUpdate { updates });
        }

        if packet_id == c_play::BLOCK_CHANGED_ACK.to_id(v) {
            let sequence = buf.get_var_int().ok()?.0;
            return Some(RecordedClientPacket::BlockChangedAck { sequence });
        }

        if packet_id == c_play::BLOCK_EVENT.to_id(v) {
            let location = buf.get_block_pos(&v).ok()?;
            let action_id = buf.get_u8().ok()?;
            let action_param = buf.get_u8().ok()?;
            let block_id = u16::try_from(buf.get_var_int().ok()?.0).unwrap_or(0);
            let block_name = format!(
                "minecraft:{}",
                pumpkin_data::BlockId::new_or_air(block_id).to_block().name
            );
            return Some(RecordedClientPacket::BlockEvent {
                pos: self.rel_block_pos(location),
                action_id,
                action_param,
                block: block_name,
            });
        }

        if packet_id == c_play::BLOCK_ENTITY_DATA.to_id(v) {
            let pkt = CBlockEntityData::read(&mut buf, &v).ok()?;
            let pos = self.rel_block_pos(pkt.location);
            if pos[0].abs() > 8 || pos[1] < -4 || pos[1] > 8 || pos[2].abs() > 8 {
                return None;
            }
            return Some(RecordedClientPacket::BlockEntityData {
                pos,
                block_entity_type: pkt.r#type.0,
                has_nbt: !pkt.nbt_data.is_empty(),
            });
        }

        if packet_id == c_play::BLOCK_DESTRUCTION.to_id(v) {
            let entity_id = buf.get_var_int().ok()?.0;
            let pos = buf.get_block_pos(&v).ok()?;
            let stage = buf.get_i8().ok()?;
            return Some(RecordedClientPacket::BlockDestruction {
                target: self.normalize_entity_id(entity_id),
                pos: self.rel_block_pos(pos),
                stage,
            });
        }

        if packet_id == c_play::LEVEL_EVENT.to_id(v) {
            let event = buf.get_i32_be().ok()?;
            let location = buf.get_block_pos(&v).ok()?;
            let data = buf.get_i32_be().ok()?;
            let disable_relative_volume = buf.get_bool().ok()?;
            return Some(RecordedClientPacket::LevelEvent {
                event,
                pos: self.rel_block_pos(location),
                data,
                disable_relative_volume,
            });
        }

        if packet_id == c_play::OPEN_SIGN_EDITOR.to_id(v) {
            let pkt = COpenSignEditor::read(&mut buf, &v).ok()?;
            return Some(RecordedClientPacket::OpenSignEditor {
                pos: self.rel_block_pos(pkt.location),
                is_front_text: pkt.is_front_text,
            });
        }

        if packet_id == c_play::OPEN_SCREEN.to_id(v) {
            let raw_win_id = buf.get_var_int().ok()?.0;
            let window_type = buf.get_var_int().ok()?.0;
            let window_id = self.normalize_window_id(raw_win_id);
            return Some(RecordedClientPacket::OpenScreen {
                window_id,
                window_type,
                title: String::new(),
            });
        }

        if packet_id == c_play::CONTAINER_CLOSE.to_id(v) {
            let pkt = CCloseContainer::read(&mut buf, &v).ok()?;
            let window_id = self.normalize_window_id(pkt.sync_id.0);
            return Some(RecordedClientPacket::CloseContainer { window_id });
        }

        if packet_id == c_play::CONTAINER_SET_CONTENT.to_id(v) {
            let pkt = CSetContainerContent::read(&mut buf, &v).ok()?;
            let window_id = self.normalize_window_id(pkt.window_id.0);
            let non_empty_slots = pkt
                .slot_data
                .iter()
                .enumerate()
                .filter_map(|(idx, slot)| {
                    RecordedItemStack::from_serializer(slot)
                        .map(|item| RecordedSlotItem { slot: idx, item })
                })
                .collect();
            let carried = RecordedItemStack::from_serializer(pkt.carried_item);
            return Some(RecordedClientPacket::ContainerSetContent {
                window_id,
                non_empty_slots,
                carried,
            });
        }

        if packet_id == c_play::CONTAINER_SET_SLOT.to_id(v) {
            let pkt = CSetContainerSlot::read(&mut buf, &v).ok()?;
            let window_id = self.normalize_window_id(i32::from(pkt.window_id));
            return Some(RecordedClientPacket::ContainerSetSlot {
                window_id,
                slot: pkt.slot,
                item: RecordedItemStack::from_serializer(pkt.slot_data),
            });
        }

        if packet_id == c_play::CONTAINER_SET_DATA.to_id(v) {
            let pkt = CSetContainerProperty::read(&mut buf, &v).ok()?;
            let window_id = self.normalize_window_id(pkt.window_id.0);
            return Some(RecordedClientPacket::ContainerSetData {
                window_id,
                property: pkt.property,
                value: pkt.value,
            });
        }

        if packet_id == c_play::SET_CURSOR_ITEM.to_id(v) {
            let stack = ItemStackSerializer::read(&mut buf).ok()?;
            return Some(RecordedClientPacket::SetCursorItem {
                item: RecordedItemStack::from_serializer(&stack),
            });
        }

        if packet_id == c_play::SET_PLAYER_INVENTORY.to_id(v) {
            let slot = buf.get_var_int().ok()?.0;
            let stack = ItemStackSerializer::read(&mut buf).ok()?;
            return Some(RecordedClientPacket::SetPlayerInventory {
                slot,
                item: RecordedItemStack::from_serializer(&stack),
            });
        }

        if packet_id == c_play::SET_HELD_SLOT.to_id(v) {
            let slot = buf.get_i8().ok()?;
            return Some(RecordedClientPacket::SetHeldSlot { slot });
        }

        if packet_id == c_play::COOLDOWN.to_id(v) {
            let group = buf.get_str().ok()?.to_string();
            let ticks = buf.get_var_int().ok()?.0;
            return Some(RecordedClientPacket::ItemCooldown { group, ticks });
        }

        if packet_id == c_play::ADD_ENTITY.to_id(v) {
            let pkt = CSpawnEntity::read_packet_data(&mut buf, &v).ok()?;
            let raw_pos = self.rel_f64_pos(pkt.position);
            if raw_pos[0].abs() > 16.0 || raw_pos[1].abs() > 16.0 || raw_pos[2].abs() > 16.0 {
                return None;
            }
            let target = self.normalize_entity_id(pkt.entity_id.0);
            let entity_type_id = u16::try_from(pkt.r#type.0).unwrap_or(u16::MAX);
            let entity_type = EntityType::from_raw(entity_type_id).map_or_else(
                || format!("entity_type_{}", pkt.r#type.0),
                |et| format!("minecraft:{}", et.resource_name),
            );
            let is_player = target == "self" || target == "observer";
            let pos = if is_player {
                raw_pos
            } else {
                [raw_pos[0].floor(), raw_pos[1].floor(), raw_pos[2].floor()]
            };
            return Some(RecordedClientPacket::SpawnEntity {
                target,
                entity_type,
                pos,
                yaw: if is_player { pkt.yaw } else { 0 },
                pitch: if is_player { pkt.pitch } else { 0 },
                head_yaw: if is_player { pkt.head_yaw } else { 0 },
                data: pkt.data.0,
                velocity: if is_player {
                    [
                        round3(pkt.velocity.0.x),
                        round3(pkt.velocity.0.y),
                        round3(pkt.velocity.0.z),
                    ]
                } else {
                    [0.0, 0.0, 0.0]
                },
            });
        }

        if packet_id == c_play::REMOVE_ENTITIES.to_id(v) {
            let count = usize::try_from(buf.get_var_int().ok()?.0).unwrap_or(0);
            let mut targets = Vec::with_capacity(count);
            for _ in 0..count {
                let eid = buf.get_var_int().ok()?.0;
                if let Some(target) = self.lookup_entity_id(eid) {
                    targets.push(target);
                }
            }
            if targets.is_empty() {
                return None;
            }
            targets.sort();
            return Some(RecordedClientPacket::RemoveEntities { targets });
        }

        if packet_id == c_play::SET_ENTITY_DATA.to_id(v) {
            let pkt = CSetEntityMetadata::read(&mut buf, &v).ok()?;
            let target = self.lookup_entity_id(pkt.entity_id.0)?;
            let shared_flags =
                (pkt.metadata.len() >= 3 && pkt.metadata[0] == 0 && pkt.metadata[1] == 0)
                    .then(|| pkt.metadata[2]);
            let raw_hex = encode_hex(&pkt.metadata);
            return Some(RecordedClientPacket::SetEntityData {
                target,
                shared_flags,
                raw_hex,
            });
        }

        if packet_id == c_play::MOVE_ENTITY_POS.to_id(v) {
            let pkt = CUpdateEntityPos::read(&mut buf, &v).ok()?;
            let target = self.lookup_entity_id(pkt.entity_id.0)?;
            if target != "self" && target != "observer" {
                return None;
            }
            if pkt.delta.x == 0 && pkt.delta.y == 0 && pkt.delta.z == 0 && pkt.on_ground {
                return None;
            }
            return Some(RecordedClientPacket::MoveEntityPos {
                target,
                delta: [pkt.delta.x, pkt.delta.y, pkt.delta.z],
                on_ground: pkt.on_ground,
            });
        }

        if packet_id == c_play::MOVE_ENTITY_POS_ROT.to_id(v) {
            let pkt = CUpdateEntityPosRot::read(&mut buf, &v).ok()?;
            let target = self.lookup_entity_id(pkt.entity_id.0)?;
            if target != "self" && target != "observer" {
                return None;
            }
            return Some(RecordedClientPacket::MoveEntityPosRot {
                target,
                delta: [pkt.delta.x, pkt.delta.y, pkt.delta.z],
                yaw: pkt.yaw,
                pitch: pkt.pitch,
                on_ground: pkt.on_ground,
            });
        }

        if packet_id == c_play::MOVE_ENTITY_ROT.to_id(v) {
            let pkt = CUpdateEntityRot::read(&mut buf, &v).ok()?;
            let target = self.lookup_entity_id(pkt.entity_id.0)?;
            if target != "self" && target != "observer" {
                return None;
            }
            return Some(RecordedClientPacket::MoveEntityRot {
                target,
                yaw: pkt.yaw,
                pitch: pkt.pitch,
                on_ground: pkt.on_ground,
            });
        }

        if packet_id == c_play::ROTATE_HEAD.to_id(v) {
            let pkt = CHeadRot::read(&mut buf, &v).ok()?;
            let target = self.lookup_entity_id(pkt.entity_id.0)?;
            if target != "self" && target != "observer" {
                return None;
            }
            return Some(RecordedClientPacket::RotateHead {
                target,
                head_yaw: pkt.head_yaw,
            });
        }

        if packet_id == c_play::ENTITY_POSITION_SYNC.to_id(v) {
            let entity_id = buf.get_var_int().ok()?.0;
            let path_type = buf.get_var_int().ok()?.0;
            let (x, y, z) = if path_type == 1 {
                let step_count = usize::try_from(buf.get_var_int().ok()?.0).ok()?;
                if step_count == 0 {
                    return None;
                }
                let mut last = (0.0, 0.0, 0.0);
                for _ in 0..step_count {
                    let sx = buf.get_f64_be().ok()?;
                    let sy = buf.get_f64_be().ok()?;
                    let sz = buf.get_f64_be().ok()?;
                    let _tick_offset = buf.get_var_int().ok()?.0;
                    last = (sx, sy, sz);
                }
                last
            } else {
                (
                    buf.get_f64_be().ok()?,
                    buf.get_f64_be().ok()?,
                    buf.get_f64_be().ok()?,
                )
            };
            let yaw = buf.get_f32_be().ok()?;
            let pitch = buf.get_f32_be().ok()?;
            let on_ground = buf.get_bool().ok()?;
            let target = self.lookup_entity_id(entity_id)?;
            if target != "self" && target != "observer" {
                return None;
            }
            return Some(RecordedClientPacket::EntityPositionSync {
                target,
                pos: self.rel_f64_pos(Vector3::new(x, y, z)),
                yaw: round3_f32(yaw),
                pitch: round3_f32(pitch),
                on_ground,
            });
        }

        if packet_id == c_play::SET_ENTITY_MOTION.to_id(v) {
            let entity_id = buf.get_var_int().ok()?.0;
            let vel = LpVector3d::read(&mut buf).ok()?.0;
            let target = self.lookup_entity_id(entity_id)?;
            if target != "self" && target != "observer" {
                return None;
            }
            return Some(RecordedClientPacket::SetEntityMotion {
                target,
                velocity: [round3(vel.x), round3(vel.y), round3(vel.z)],
            });
        }

        if packet_id == c_play::SET_EQUIPMENT.to_id(v) {
            let pkt = CSetEquipment::read(&mut buf, &v).ok()?;
            let target = self.lookup_entity_id(pkt.entity_id.0)?;
            let slots = pkt
                .equipment
                .iter()
                .map(|(slot, stack)| RecordedEquipmentSlot {
                    slot: *slot,
                    item: RecordedItemStack::from_serializer(stack),
                })
                .collect();
            return Some(RecordedClientPacket::SetEquipment { target, slots });
        }

        if packet_id == c_play::ENTITY_EVENT.to_id(v) {
            let entity_id = buf.get_i32_be().ok()?;
            let status = buf.get_i8().ok()?;
            let target = self.lookup_entity_id(entity_id)?;
            return Some(RecordedClientPacket::EntityEvent { target, status });
        }

        if packet_id == c_play::ANIMATE.to_id(v) {
            let entity_id = buf.get_var_int().ok()?.0;
            let anim_id = buf.get_u8().ok()?;
            let target = self.lookup_entity_id(entity_id)?;
            let animation = match anim_id {
                0 => "LeaveBed".to_string(),
                1 => "CriticalEffect".to_string(),
                2 => "MagicCriticalEffect".to_string(),
                other => format!("Anim_{other}"),
            };
            return Some(RecordedClientPacket::Animate { target, animation });
        }

        if packet_id == c_play::SWING_ANIMATION.to_id(v) {
            let entity_id = buf.get_var_int().ok()?.0;
            let off_hand = buf.get_var_int().ok()?.0 != 0;
            let target = self.lookup_entity_id(entity_id)?;
            let animation = if off_hand {
                "SwingOffhand".to_string()
            } else {
                "SwingMainArm".to_string()
            };
            return Some(RecordedClientPacket::Animate { target, animation });
        }

        if packet_id == c_play::HURT_ANIMATION.to_id(v) {
            let entity_id = buf.get_var_int().ok()?.0;
            let yaw = buf.get_f32_be().ok()?;
            let target = self.lookup_entity_id(entity_id)?;
            return Some(RecordedClientPacket::HurtAnimation {
                target,
                yaw: round3_f32(yaw),
            });
        }

        if packet_id == c_play::DAMAGE_EVENT.to_id(v) {
            let entity_id = buf.get_var_int().ok()?.0;
            let source_type_id = buf.get_var_int().ok()?.0;
            let source_cause_id = buf.get_var_int().ok()?.0;
            let source_direct_id = buf.get_var_int().ok()?.0;
            let target = self.lookup_entity_id(entity_id)?;
            return Some(RecordedClientPacket::DamageEvent {
                target,
                source_type_id,
                has_cause: source_cause_id > 0,
                has_direct: source_direct_id > 0,
            });
        }

        if packet_id == c_play::TAKE_ITEM_ENTITY.to_id(v) {
            let pkt = CTakeItemEntity::read(&mut buf, &v).ok()?;
            let item_target = self.lookup_entity_id(pkt.entity_id.0)?;
            let collector_target = self.lookup_entity_id(pkt.collector_entity_id.0)?;
            return Some(RecordedClientPacket::TakeItemEntity {
                item_target,
                collector_target,
                amount: pkt.stack_amount.0,
            });
        }

        if packet_id == c_play::SET_HEALTH.to_id(v) {
            let pkt = CSetHealth::read(&mut buf, &v).ok()?;
            return Some(RecordedClientPacket::SetHealth {
                health: round3_f32(pkt.health),
                food: pkt.food.0,
                saturation: round3_f32(pkt.food_saturation),
            });
        }

        if packet_id == c_play::SET_EXPERIENCE.to_id(v) {
            let pkt = CSetExperience::read(&mut buf, &v).ok()?;
            return Some(RecordedClientPacket::SetExperience {
                progress: round3_f32(pkt.progress),
                level: pkt.level.0,
                total: pkt.total_experience.0,
            });
        }

        if packet_id == c_play::UPDATE_ATTRIBUTES.to_id(v) {
            let pkt = CUpdateAttributes::read(&mut buf, &v).ok()?;
            let target = self.lookup_entity_id(pkt.entity_id.0)?;
            let mut attributes: Vec<RecordedAttribute> = pkt
                .properties
                .iter()
                .map(|prop| RecordedAttribute {
                    id: prop.id.0,
                    base_value: round3(prop.value),
                    modifier_count: prop.modifiers.len(),
                })
                .collect();
            attributes.sort_by_key(|a| a.id);
            return Some(RecordedClientPacket::UpdateAttributes { target, attributes });
        }

        if packet_id == c_play::UPDATE_MOB_EFFECT.to_id(v) {
            let entity_id = buf.get_var_int().ok()?.0;
            let effect_id = buf.get_var_int().ok()?.0;
            let amplifier = buf.get_var_int().ok()?.0;
            let duration = buf.get_var_int().ok()?.0;
            let flags = buf.get_i8().ok()?;
            let target = self.lookup_entity_id(entity_id)?;
            return Some(RecordedClientPacket::UpdateMobEffect {
                target,
                effect_id,
                amplifier,
                duration,
                flags,
            });
        }

        if packet_id == c_play::REMOVE_MOB_EFFECT.to_id(v) {
            let entity_id = buf.get_var_int().ok()?.0;
            let effect_id = buf.get_var_int().ok()?.0;
            let target = self.lookup_entity_id(entity_id)?;
            return Some(RecordedClientPacket::RemoveMobEffect { target, effect_id });
        }

        if packet_id == c_play::PLAYER_ABILITIES.to_id(v) {
            let flags = buf.get_i8().ok()?;
            let flying_speed = buf.get_f32_be().ok()?;
            let field_of_view = buf.get_f32_be().ok()?;
            return Some(RecordedClientPacket::PlayerAbilities {
                flags,
                flying_speed: round3_f32(flying_speed),
                field_of_view: round3_f32(field_of_view),
            });
        }

        if packet_id == c_play::GAME_EVENT.to_id(v) {
            let event = buf.get_u8().ok()?;
            let value = buf.get_f32_be().ok()?;
            return Some(RecordedClientPacket::GameEvent {
                event,
                value: round3_f32(value),
            });
        }

        if packet_id == c_play::PLAYER_POSITION.to_id(v) {
            let pkt = CPlayerPosition::read(&mut buf, &v).ok()?;
            let rx = if pkt.relatives.contains(&PositionFlag::X) {
                round3(pkt.position.x)
            } else {
                round3(pkt.position.x - f64::from(self.origin.0.x))
            };
            let ry = if pkt.relatives.contains(&PositionFlag::Y) {
                round3(pkt.position.y)
            } else {
                round3(pkt.position.y - f64::from(self.origin.0.y))
            };
            let rz = if pkt.relatives.contains(&PositionFlag::Z) {
                round3(pkt.position.z)
            } else {
                round3(pkt.position.z - f64::from(self.origin.0.z))
            };
            return Some(RecordedClientPacket::PlayerPosition {
                pos: [rx, ry, rz],
                delta: [
                    round3(pkt.delta.x),
                    round3(pkt.delta.y),
                    round3(pkt.delta.z),
                ],
                yaw: round3_f32(pkt.yaw),
                pitch: round3_f32(pkt.pitch),
                relatives_count: pkt.relatives.len(),
            });
        }

        if packet_id == c_play::PLAYER_ROTATION.to_id(v) {
            let yaw = buf.get_f32_be().ok()?;
            let is_yaw_relative = buf.get_bool().ok()?;
            let pitch = buf.get_f32_be().ok()?;
            let is_pitch_relative = buf.get_bool().ok()?;
            return Some(RecordedClientPacket::PlayerRotation {
                yaw: round3_f32(yaw),
                is_yaw_relative,
                pitch: round3_f32(pitch),
                is_pitch_relative,
            });
        }

        if packet_id == c_play::RESPAWN.to_id(v) {
            let pkt = CRespawn::read(&mut buf, &v).ok()?;
            return Some(RecordedClientPacket::Respawn {
                gamemode: pkt.player_spawn_info.game_mode,
                dimension: pkt.player_spawn_info.dimension.minecraft_name.to_string(),
            });
        }

        if packet_id == c_play::TICKING_STATE.to_id(v) {
            let tick_rate = buf.get_f32_be().ok()?;
            let is_frozen = buf.get_bool().ok()?;
            return Some(RecordedClientPacket::TickingState {
                tick_rate: round3_f32(tick_rate),
                is_frozen,
            });
        }

        if packet_id == c_play::TICKING_STEP.to_id(v) {
            let tick_steps = buf.get_var_int().ok()?.0;
            return Some(RecordedClientPacket::TickingStep { tick_steps });
        }

        if self.include_sounds && packet_id == c_play::SOUND.to_id(v) {
            let sound_holder = IdOr::<SoundEvent>::read(&mut buf, |r| {
                let sound_name = r.get_str()?.to_string();
                let range = r.get_option(NetworkReadExt::get_f32_be)?;
                Ok(SoundEvent { sound_name, range })
            })
            .ok()?;
            let sound = match sound_holder {
                IdOr::Id(id) => pumpkin_data::sound::Sound::NAMES
                    .get(usize::from(id))
                    .copied()
                    .map_or_else(|| format!("sound_id_{id}"), |s| format!("minecraft:{s}")),
                IdOr::Value(ev) => ev.sound_name,
            };
            let category = VarInt::decode(&mut buf).ok()?.0;
            let x = buf.get_i32_be().ok()? / 8 - self.origin.0.x;
            let y = buf.get_i32_be().ok()? / 8 - self.origin.0.y;
            let z = buf.get_i32_be().ok()? / 8 - self.origin.0.z;
            return Some(RecordedClientPacket::Sound {
                sound,
                category,
                pos: [x, y, z],
            });
        }

        Some(RecordedClientPacket::UnknownGameplayPacket {
            packet_id,
            payload_len: payload.len(),
        })
    }
}

#[must_use]
fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(char::from(HEX[usize::from(b >> 4)]));
        out.push(char::from(HEX[usize::from(b & 0x0f)]));
    }
    out
}

/// Sorts contiguous runs of `SetEntityData` packets by `(target, raw_hex)` so concurrent
/// rayon entity metadata broadcasts within the same tick have a canonical order.
pub fn canonicalize_tick_packets(packets: &mut [RecordedClientPacket]) {
    let mut start = 0;
    while start < packets.len() {
        if matches!(packets[start], RecordedClientPacket::SetEntityData { .. }) {
            let mut end = start + 1;
            while end < packets.len()
                && matches!(packets[end], RecordedClientPacket::SetEntityData { .. })
            {
                end += 1;
            }
            if end - start > 1 {
                packets[start..end].sort_by(|a, b| match (a, b) {
                    (
                        RecordedClientPacket::SetEntityData {
                            target: ta,
                            raw_hex: ha,
                            ..
                        },
                        RecordedClientPacket::SetEntityData {
                            target: tb,
                            raw_hex: hb,
                            ..
                        },
                    ) => (ta, ha).cmp(&(tb, hb)),
                    _ => std::cmp::Ordering::Equal,
                });
            }
            start = end;
        } else {
            start += 1;
        }
    }
}
