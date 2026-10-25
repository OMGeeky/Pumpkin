use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, PoisonError, atomic::Ordering},
    time::Duration,
};

use pumpkin_core::{
    block::entities::block_entity_from_nbt,
    entity::player::Player,
    net::ClientPlatform,
    server::Server,
    world::{BlockFlags, World},
};
use pumpkin_data::{
    Block, BlockStateId, data_component_impl::EquipmentSlot, dimension::Dimension,
    entity::EntityType, item::Item, item_stack::ItemStack,
};
use pumpkin_gametest::format_canonical_block_state;
use pumpkin_protocol::java::client::play::CPingResponse;
use pumpkin_util::{
    GameMode, PermissionLvl,
    math::{position::BlockPos, vector2::Vector2, vector3::Vector3},
};
use serde::{Deserialize, Serialize};
use tempfile::TempDir;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    process::{Child, ChildStdin, Command},
    sync::mpsc::{UnboundedReceiver, unbounded_channel},
    time::Instant,
};
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use uuid::Uuid;

use crate::{
    harness::{copy_dir_recursive, create_test_server_with_world, spawn_java_listener},
    lockstep::{
        client::{BotTarget, LockstepAction, LockstepBot},
        packets::{
            PacketNormalizer, RecordedBlockUpdate, RecordedClientPacket, RecordedItemStack,
            RecordedSlotItem, canonicalize_tick_packets, round3, round3_f32,
        },
        scenarios::{
            LockstepScenario, default_lockstep_structures_dir, load_structure_template,
            resolve_structure_nbt_path,
        },
    },
};

pub const PRIMARY_BOT_UUID: Uuid = Uuid::from_u128(0x10C5_7E90_0000_4000_8000_0000_0000_0001);
pub const OBSERVER_BOT_UUID: Uuid = Uuid::from_u128(0x10C5_7E90_0000_4000_8000_0000_0000_0002);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LockstepWorldSnapshot {
    pub player_pos: [f64; 3],
    pub yaw: f32,
    pub pitch: f32,
    pub on_ground: bool,
    pub sprinting: bool,
    pub sneaking: bool,
    pub flying: bool,
    pub gamemode: String,
    pub selected_slot: u8,
    pub health: f32,
    pub food: i32,
    pub inventory: Vec<RecordedSlotItem>,
    pub watched_blocks: Vec<RecordedBlockUpdate>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitialBlockPlacement {
    pub offset: [i32; 3],
    #[serde(with = "canonical_state_serde", rename = "state")]
    pub state_id: BlockStateId,
}

mod canonical_state_serde {
    use pumpkin_data::BlockStateId;
    use pumpkin_gametest::{format_canonical_block_state, parse_canonical_block_state};
    use serde::{Deserialize, Deserializer, Serializer};

    #[allow(clippy::trivially_copy_pass_by_ref)]
    pub fn serialize<S: Serializer>(
        state_id: &BlockStateId,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format_canonical_block_state(*state_id))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<BlockStateId, D::Error> {
        let s = String::deserialize(deserializer)?;
        parse_canonical_block_state(&s).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitialInventoryItem {
    pub slot: usize,
    pub item: String,
    pub count: u8,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InitialSpawnedMob {
    pub entity_type: String,
    pub offset: [f64; 3],
}

/// An in-process Pumpkin server endpoint for deterministic lockstep execution.
pub struct InProcessPumpkinPeer {
    pub server: Arc<Server>,
    pub world: Arc<World>,
    pub origin: BlockPos,
    pub primary_bot: LockstepBot,
    pub primary_player: Arc<Player>,
    pub observer_bot: LockstepBot,
    pub observer_player: Arc<Player>,
    pub normalizer_primary: PacketNormalizer,
    pub normalizer_observer: PacketNormalizer,
    pub barrier_counter: i64,
    cancel: CancellationToken,
    tasks: TaskTracker,
    _temp_dir: TempDir,
}

impl InProcessPumpkinPeer {
    #[allow(clippy::too_many_lines)]
    pub async fn boot_with_world(
        origin: BlockPos,
        world_save: Option<&Path>,
    ) -> Result<Self, String> {
        let temp_dir =
            tempfile::tempdir().map_err(|e| format!("Failed to create lockstep temp dir: {e}"))?;
        let server = create_test_server_with_world(&temp_dir, world_save).await?;
        let world = server.get_world_from_dimension(&Dimension::OVERWORLD);

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| format!("Failed to bind lockstep TCP listener: {e}"))?;
        let addr = listener
            .local_addr()
            .map_err(|e| format!("Failed to query lockstep TCP address: {e}"))?;
        let cancel = CancellationToken::new();
        let tasks = TaskTracker::new();
        spawn_java_listener(listener, server.clone(), cancel.clone(), &tasks);

        // Configure deterministic gamerules and world spawn at `origin`
        let mut level_info = world.level_info.load().as_ref().clone();
        level_info.spawn_x = origin.0.x;
        level_info.spawn_y = origin.0.y;
        level_info.spawn_z = origin.0.z;
        level_info.game_rules.advance_time = false;
        level_info.game_rules.advance_weather = false;
        level_info.game_rules.spawn_mobs = false;
        level_info.game_rules.random_tick_speed = 0;
        level_info.game_rules.natural_health_regeneration = false;
        level_info.game_rules.send_command_feedback = false;
        world.level_info.store(Arc::new(level_info));

        // Force-load and fetch the 3x3 chunk region around `origin`
        let base_cx = origin.0.x >> 4;
        let base_cz = origin.0.z >> 4;
        register_forced_arena_chunks(&world, base_cx, base_cz);
        for cx in (base_cx - 1)..=(base_cx + 1) {
            for cz in (base_cz - 1)..=(base_cz + 1) {
                world
                    .level
                    .get_or_fetch_chunk(Vector2::new(cx, cz), |_| ())
                    .await;
            }
        }
        world.update_active_chunks();

        if world_save.is_none() {
            // Build initial stone floor at `origin.y - 1` before connecting bots
            let stone_id = Block::STONE.default_state.id;
            let air_id = Block::AIR.default_state.id;
            let clear_flags = BlockFlags::FORCE_STATE
                | BlockFlags::SKIP_BLOCK_ADDED_CALLBACK
                | BlockFlags::SKIP_BLOCK_ENTITY_REPLACED_CALLBACK;
            for dx in -4..=4 {
                for dz in -4..=4 {
                    let floor_pos = BlockPos::new(origin.0.x + dx, origin.0.y - 1, origin.0.z + dz);
                    let _ = world.set_block_state(&floor_pos, stone_id, clear_flags);
                    for dy in 0..=5 {
                        let air_pos =
                            BlockPos::new(origin.0.x + dx, origin.0.y + dy, origin.0.z + dz);
                        let _ = world.set_block_state(&air_pos, air_id, clear_flags);
                    }
                }
            }
            let obs_park_floor = BlockPos::new(origin.0.x, origin.0.y - 1, origin.0.z + 10);
            let _ = world.set_block_state(&obs_park_floor, stone_id, clear_flags);
        }

        // Connect primary and observer bots once
        let primary_bot =
            LockstepBot::connect_and_login(addr, "LockBot1", PRIMARY_BOT_UUID).await?;
        let primary_player = wait_for_player(&world, "LockBot1").await?;
        let dispatcher = server.command_dispatcher.load();
        primary_player.set_permission_lvl(&server, PermissionLvl::Four, &dispatcher);

        let observer_bot =
            LockstepBot::connect_and_login(addr, "LockBot2", OBSERVER_BOT_UUID).await?;
        let observer_player = wait_for_player(&world, "LockBot2").await?;

        // Process initial SConfirmTeleport and SPlayerLoaded packets from both bots
        wait_for_inbound_packets(&primary_player, 2).await;
        wait_for_inbound_packets(&observer_player, 2).await;
        primary_player.process_inbound_packets();
        observer_player.process_inbound_packets();
        server.tick();

        let normalizer_primary = PacketNormalizer::new(
            origin,
            primary_bot.entity_id,
            Some(observer_bot.entity_id),
            false,
        );
        let normalizer_observer = PacketNormalizer::new(
            origin,
            observer_bot.entity_id,
            Some(primary_bot.entity_id),
            false,
        );

        let mut peer = Self {
            server,
            world,
            origin,
            primary_bot,
            primary_player,
            observer_bot,
            observer_player,
            normalizer_primary,
            normalizer_observer,
            barrier_counter: 10_000,
            cancel,
            tasks,
            _temp_dir: temp_dir,
        };

        // Run 5 warmup ticks so initial chunk batches are delivered, acknowledged, and entity-paired
        for _ in 0..5 {
            let _ = peer.step_tick(&[], &[]).await?;
        }

        Ok(peer)
    }

    #[expect(
        clippy::too_many_lines,
        reason = "Clears entities, resets arena blocks, places structures, and resets player state"
    )]
    pub async fn reset_for_scenario(&mut self, scenario: &LockstepScenario) -> Result<(), String> {
        // 1. Clear all non-player entities in the world (unless testing a pre-populated world savegame)
        if scenario.world_save.is_none() {
            let entities = self.world.entities.load_full();
            for entity in entities.iter() {
                if entity.get_entity().entity_type != &EntityType::PLAYER {
                    self.world.remove_entity(entity.as_ref());
                }
            }
        }

        // 2. Ensure tick rate manager is unfrozen at 20 TPS and clear scheduled ticks/block events in arena
        self.server
            .tick_rate_manager
            .set_tick_rate(&self.server, 20.0);
        self.server
            .tick_rate_manager
            .set_frozen(&self.server, false);
        self.server.tick_rate_manager.tick();
        if scenario.world_save.is_none() {
            let min_pos = BlockPos::new(
                self.origin.0.x - 16,
                self.origin.0.y - 4,
                self.origin.0.z - 16,
            );
            let max_pos = BlockPos::new(
                self.origin.0.x + 16,
                self.origin.0.y + 16,
                self.origin.0.z + 16,
            );
            let base_cx = self.origin.0.x >> 4;
            let base_cz = self.origin.0.z >> 4;
            for cx in (base_cx - 1)..=(base_cx + 1) {
                for cz in (base_cz - 1)..=(base_cz + 1) {
                    let chunk_pos = Vector2::new(cx, cz);
                    if let Some(chunk) = self.world.level.loaded_chunks.get(&chunk_pos) {
                        chunk.block_ticks.clear_area(&min_pos, &max_pos);
                        chunk.fluid_ticks.clear_area(&min_pos, &max_pos);
                        if !chunk.block_ticks.has_ticks() && !chunk.fluid_ticks.has_ticks() {
                            self.world
                                .level
                                .chunks_with_scheduled_ticks
                                .remove(&chunk_pos);
                        }
                    }
                }
            }
            self.world
                .clear_synced_block_events_in_box(&min_pos, &max_pos);

            // 3. Build solid 9x9 stone floor under `origin` (at origin.y - 1) and clear space above
            let stone_id = Block::STONE.default_state.id;
            let air_id = Block::AIR.default_state.id;
            let clear_flags = BlockFlags::FORCE_STATE
                | BlockFlags::SKIP_DROPS
                | BlockFlags::SKIP_BLOCK_ADDED_CALLBACK
                | BlockFlags::SKIP_BLOCK_ENTITY_REPLACED_CALLBACK;
            for dx in -4..=4 {
                for dz in -4..=4 {
                    for dy in (0..=5).rev() {
                        let air_pos = BlockPos::new(
                            self.origin.0.x + dx,
                            self.origin.0.y + dy,
                            self.origin.0.z + dz,
                        );
                        self.world.remove_block_entity(&air_pos);
                        let _ = self.world.set_block_state(&air_pos, air_id, clear_flags);
                    }
                    let floor_pos = BlockPos::new(
                        self.origin.0.x + dx,
                        self.origin.0.y - 1,
                        self.origin.0.z + dz,
                    );
                    self.world.remove_block_entity(&floor_pos);
                    let _ = self
                        .world
                        .set_block_state(&floor_pos, stone_id, clear_flags);
                }
            }

            // Ensure solid floor under observer parking spot at origin + [0, -1, 10]
            let obs_park_floor =
                BlockPos::new(self.origin.0.x, self.origin.0.y - 1, self.origin.0.z + 10);
            let _ = self
                .world
                .set_block_state(&obs_park_floor, stone_id, clear_flags);
        }

        // 4. Apply scenario `.nbt` structure template (if any) and initial block placements
        let init_block_flags =
            BlockFlags::FORCE_STATE | BlockFlags::NOTIFY_LISTENERS | BlockFlags::SKIP_DROPS;
        if let Some(structure_id) = &scenario.structure {
            let template = load_structure_template(structure_id)?;
            let base_x = self.origin.0.x + scenario.structure_offset[0];
            let base_y = self.origin.0.y + scenario.structure_offset[1];
            let base_z = self.origin.0.z + scenario.structure_offset[2];
            for block in template.blocks() {
                let pos = BlockPos::new(
                    base_x + block.position[0],
                    base_y + block.position[1],
                    base_z + block.position[2],
                );
                self.world.remove_block_entity(&pos);
                let _ = self
                    .world
                    .set_block_state(&pos, block.state, init_block_flags);
                if let Some(nbt) = &block.nbt {
                    let mut nbt_with_pos = nbt.clone();
                    nbt_with_pos.put_int("x", pos.0.x);
                    nbt_with_pos.put_int("y", pos.0.y);
                    nbt_with_pos.put_int("z", pos.0.z);
                    if let Some(entity) = block_entity_from_nbt(&nbt_with_pos) {
                        self.world.add_block_entity(entity);
                    }
                }
            }
        }
        for placement in &scenario.initial_blocks {
            let pos = BlockPos::new(
                self.origin.0.x + placement.offset[0],
                self.origin.0.y + placement.offset[1],
                self.origin.0.z + placement.offset[2],
            );
            let _ = self
                .world
                .set_block_state(&pos, placement.state_id, init_block_flags);
        }
        self.world.flush_block_updates();

        // Remove any non-player entities produced during block setup
        if scenario.world_save.is_none() {
            let entities = self.world.entities.load_full();
            for entity in entities.iter() {
                if entity.get_entity().entity_type != &EntityType::PLAYER {
                    self.world.remove_entity(entity.as_ref());
                }
            }
        }

        // 5 & 6. Reset primary and observer player state synchronously
        self.reset_player_state(scenario);

        // 7. Spawn initial scenario mobs
        self.primary_bot.spawned_entity_ids.clear();
        self.observer_bot.spawned_entity_ids.clear();
        let mut spawned_mob_ids = Vec::new();
        for (idx, mob_spec) in scenario.initial_mobs.iter().enumerate() {
            let stripped = mob_spec
                .entity_type
                .strip_prefix("minecraft:")
                .unwrap_or(&mob_spec.entity_type);
            if let Some(et) = EntityType::from_name(stripped) {
                let mob_pos = Vector3::new(
                    f64::from(self.origin.0.x) + mob_spec.offset[0],
                    f64::from(self.origin.0.y) + mob_spec.offset[1],
                    f64::from(self.origin.0.z) + mob_spec.offset[2],
                );
                let entity = pumpkin_core::entity::r#type::from_type(
                    et,
                    mob_pos,
                    &self.world,
                    Uuid::from_u128(0x10C5_7E90_0000_4000_8000_0000_0000_1000 + idx as u128),
                );
                let eid = entity.get_entity().entity_id;
                let _ = self.world.spawn_entity(entity);
                spawned_mob_ids.push(eid);
            }
        }

        // 8. Run 2 warmup ticks to flush setup packets, then reset normalizers & sequence counters
        for _ in 0..2 {
            let _ = self.step_tick(&[], &[]).await?;
        }
        while self.primary_player.inbound_packets.pop().is_some() {}
        while self.observer_player.inbound_packets.pop().is_some() {}
        self.primary_bot.pending_auto_sent = 0;
        self.observer_bot.pending_auto_sent = 0;
        if let ClientPlatform::Java(c) = self.primary_player.client.as_ref() {
            c.packet_sequence.store(-1, Ordering::Relaxed);
        }
        if let ClientPlatform::Java(c) = self.observer_player.client.as_ref() {
            c.packet_sequence.store(-1, Ordering::Relaxed);
        }

        self.primary_bot.sequence_counter = 0;
        self.primary_bot.active_window_id = None;
        self.observer_bot.sequence_counter = 0;
        self.observer_bot.active_window_id = None;

        self.normalizer_primary = PacketNormalizer::new(
            self.origin,
            self.primary_bot.entity_id,
            Some(self.observer_bot.entity_id),
            scenario.include_sounds,
        );
        self.normalizer_observer = PacketNormalizer::new(
            self.origin,
            self.observer_bot.entity_id,
            Some(self.primary_bot.entity_id),
            scenario.include_sounds,
        );

        for (idx, &mob_id) in spawned_mob_ids.iter().enumerate() {
            self.primary_bot.spawned_entity_ids.push(mob_id);
            self.observer_bot.spawned_entity_ids.push(mob_id);
            self.normalizer_primary
                .register_entity_label(mob_id, format!("mob_{idx}"));
            self.normalizer_observer
                .register_entity_label(mob_id, format!("mob_{idx}"));
        }

        Ok(())
    }

    #[expect(
        clippy::too_many_lines,
        reason = "Resets primary and observer player attributes, abilities, and inventories"
    )]
    fn reset_player_state(&mut self, scenario: &LockstepScenario) {
        while self.primary_player.inbound_packets.pop().is_some() {}
        while self.observer_player.inbound_packets.pop().is_some() {}
        *self
            .primary_player
            .awaiting_teleport
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
        *self
            .observer_player
            .awaiting_teleport
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
        self.primary_player.set_client_loaded(true);
        self.observer_player.set_client_loaded(true);
        self.primary_player
            .chat_spam_tick_count
            .store(0, Ordering::Relaxed);
        self.observer_player
            .chat_spam_tick_count
            .store(0, Ordering::Relaxed);
        self.primary_player.tick_counter.store(0, Ordering::Relaxed);
        self.observer_player
            .tick_counter
            .store(0, Ordering::Relaxed);
        self.primary_player
            .last_attacked_ticks
            .store(100, Ordering::Relaxed);
        self.observer_player
            .last_attacked_ticks
            .store(100, Ordering::Relaxed);
        self.primary_player.mining.store(false, Ordering::Relaxed);
        self.primary_player
            .current_block_destroy_stage
            .store(-1, Ordering::Relaxed);
        self.primary_player
            .start_mining_time
            .store(0, Ordering::Relaxed);

        self.primary_player.open_container.store(None);
        self.primary_player.open_container_pos.store(None);
        *self
            .primary_player
            .current_screen_handler
            .lock()
            .unwrap_or_else(PoisonError::into_inner) =
            self.primary_player.player_screen_handler.clone();
        self.primary_player
            .screen_handler_sync_id
            .store(0, Ordering::Relaxed);
        *self
            .primary_player
            .carried_item
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
        let primary_spawn = Vector3::new(
            f64::from(self.origin.0.x) + 0.5,
            f64::from(self.origin.0.y),
            f64::from(self.origin.0.z) + 0.5,
        );
        self.primary_player
            .living_entity
            .entity
            .set_pos(primary_spawn);
        self.primary_player
            .living_entity
            .entity
            .set_rotation(0.0, 0.0);
        self.primary_bot.pos = primary_spawn;
        self.primary_bot.yaw = 0.0;
        self.primary_bot.pitch = 0.0;
        self.primary_player
            .living_entity
            .entity
            .velocity
            .store(Vector3::default());
        self.primary_player
            .living_entity
            .entity
            .on_ground
            .store(true, Ordering::Relaxed);
        self.primary_player
            .living_entity
            .entity
            .set_sprinting(false);
        self.primary_player.living_entity.entity.set_sneaking(false);
        self.primary_player.living_entity.set_health(20.0);
        self.primary_player
            .last_sent_health
            .store(20, Ordering::Relaxed);
        self.primary_player
            .living_entity
            .hurt_cooldown
            .store(0, Ordering::Relaxed);
        self.primary_player
            .living_entity
            .last_damage_taken
            .store(0.0);
        self.primary_player.hunger_manager.level.store(20);
        self.primary_player.hunger_manager.saturation.store(5.0);
        self.primary_player
            .last_sent_food
            .store(20, Ordering::Relaxed);
        self.primary_player
            .last_food_saturation
            .store(true, Ordering::Relaxed);
        self.primary_player.living_entity.clear_active_hand();
        self.primary_player
            .living_entity
            .active_effects
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        self.primary_player
            .experience_level
            .store(0, Ordering::Relaxed);
        self.primary_player
            .experience_points
            .store(0, Ordering::Relaxed);
        self.primary_player.experience_progress.store(0.0);
        self.primary_player.last_sent_xp.store(0, Ordering::Relaxed);

        let _ = self.primary_player.set_gamemode(scenario.initial_gamemode);
        let mut abilities = self
            .primary_player
            .abilities
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        abilities.flying = false;
        abilities.allow_flying = matches!(
            scenario.initial_gamemode,
            GameMode::Creative | GameMode::Spectator
        );
        abilities.creative = scenario.initial_gamemode == GameMode::Creative;
        abilities.invulnerable = matches!(
            scenario.initial_gamemode,
            GameMode::Creative | GameMode::Spectator
        );
        drop(abilities);

        // Clear and repopulate primary player inventory + equipment
        self.primary_player.inventory.set_selected_slot(0);
        {
            let mut main_inv = self
                .primary_player
                .inventory
                .main_inventory
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            for slot in main_inv.iter_mut() {
                *slot = ItemStack::EMPTY.clone();
            }
            for item_spec in &scenario.initial_inventory {
                let stripped = item_spec
                    .item
                    .strip_prefix("minecraft:")
                    .unwrap_or(&item_spec.item);
                if let Some(item_def) = Item::from_registry_key(stripped)
                    && let Some(slot_ref) = main_inv.get_mut(item_spec.slot)
                {
                    *slot_ref = ItemStack::new(item_spec.count, item_def);
                }
            }
        }
        {
            let mut equip = self
                .primary_player
                .inventory
                .entity_equipment
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            for slot in [
                EquipmentSlot::MAIN_HAND,
                EquipmentSlot::OFF_HAND,
                EquipmentSlot::FEET,
                EquipmentSlot::LEGS,
                EquipmentSlot::CHEST,
                EquipmentSlot::HEAD,
                EquipmentSlot::BODY,
                EquipmentSlot::SADDLE,
            ] {
                equip.put(&slot, ItemStack::EMPTY.clone());
            }
        }
        self.primary_player.sync_inventory_to_client();

        let obs_pos = if scenario.spawn_observer {
            Vector3::new(
                f64::from(self.origin.0.x) + scenario.observer_offset[0],
                f64::from(self.origin.0.y) + scenario.observer_offset[1],
                f64::from(self.origin.0.z) + scenario.observer_offset[2],
            )
        } else {
            Vector3::new(
                f64::from(self.origin.0.x) + 0.5,
                f64::from(self.origin.0.y),
                f64::from(self.origin.0.z) + 10.5,
            )
        };
        self.observer_player.living_entity.entity.set_pos(obs_pos);
        self.observer_player
            .living_entity
            .entity
            .set_rotation(180.0, 0.0);
        self.observer_bot.pos = obs_pos;
        self.observer_bot.yaw = 180.0;
        self.observer_bot.pitch = 0.0;
        self.observer_player.living_entity.set_health(20.0);
        self.observer_player
            .last_sent_health
            .store(20, Ordering::Relaxed);
        self.observer_player
            .living_entity
            .hurt_cooldown
            .store(0, Ordering::Relaxed);
        self.observer_player
            .living_entity
            .last_damage_taken
            .store(0.0);
        self.observer_player
            .living_entity
            .entity
            .on_ground
            .store(true, Ordering::Relaxed);
        let _ = self.observer_player.set_gamemode(GameMode::Survival);
    }

    pub async fn step_tick(
        &mut self,
        actions: &[(BotTarget, LockstepAction)],
        watched_offsets: &[[i32; 3]],
    ) -> Result<
        (
            Vec<RecordedClientPacket>,
            Vec<RecordedClientPacket>,
            LockstepWorldSnapshot,
        ),
        String,
    > {
        if !self.primary_player.inbound_packets.is_empty() {
            self.primary_player.process_inbound_packets();
        }
        if !self.observer_player.inbound_packets.is_empty() {
            self.observer_player.process_inbound_packets();
        }

        let obs_eid = Some(self.observer_bot.entity_id);
        let prim_eid = Some(self.primary_bot.entity_id);

        let mut primary_sent = 0usize;
        let mut observer_sent = 0usize;

        for (target, action) in actions {
            match target {
                BotTarget::Primary => {
                    self.primary_bot
                        .execute_action(action, self.origin, obs_eid)
                        .await?;
                    primary_sent += 1;
                }
                BotTarget::Observer => {
                    self.observer_bot
                        .execute_action(action, self.origin, prim_eid)
                        .await?;
                    observer_sent += 1;
                }
            }
        }

        if primary_sent > 0 {
            wait_for_inbound_packets(&self.primary_player, primary_sent).await;
        }
        if observer_sent > 0 {
            wait_for_inbound_packets(&self.observer_player, observer_sent).await;
        }

        // Process inbound packets sequentially (primary first, then observer)
        self.primary_player.process_inbound_packets();
        self.observer_player.process_inbound_packets();

        // Advance the server by exactly 1 tick (matching Ticker::run)
        self.server.tick_rate_manager.tick();
        if self.server.tick_rate_manager.is_sprinting() {
            self.server.tick_rate_manager.start_sprint_tick_work();
            self.server.tick();
            if self.server.tick_rate_manager.end_sprint_tick_work() {
                self.server
                    .tick_rate_manager
                    .finish_tick_sprint(&self.server);
            }
        } else {
            self.server.tick();
        }

        // Enqueue an end-of-tick CPingResponse barrier on the primary and observer outgoing channels
        self.barrier_counter += 1;
        let nonce_primary = self.barrier_counter;
        if let ClientPlatform::Java(java_client) = self.primary_player.client.as_ref() {
            java_client.try_send_packet(&CPingResponse::new(nonce_primary));
            java_client.flush_channel();
        }

        let raw_primary = self
            .primary_bot
            .read_until_pong_nonce(nonce_primary, Duration::from_secs(5))
            .await?;

        let auto_primary = std::mem::take(&mut self.primary_bot.pending_auto_sent);
        if auto_primary > 0 {
            wait_for_inbound_packets(&self.primary_player, auto_primary).await;
            self.primary_player.process_inbound_packets();
        }

        let mut decoded_primary: Vec<RecordedClientPacket> = raw_primary
            .into_iter()
            .filter_map(|(id, payload)| self.normalizer_primary.decode_packet(id, &payload))
            .collect();
        canonicalize_tick_packets(&mut decoded_primary);

        self.barrier_counter += 1;
        let nonce_obs = self.barrier_counter;
        if let ClientPlatform::Java(java_client) = self.observer_player.client.as_ref() {
            java_client.try_send_packet(&CPingResponse::new(nonce_obs));
            java_client.flush_channel();
        }
        let raw_obs = self
            .observer_bot
            .read_until_pong_nonce(nonce_obs, Duration::from_secs(5))
            .await?;
        let auto_obs = std::mem::take(&mut self.observer_bot.pending_auto_sent);
        if auto_obs > 0 {
            wait_for_inbound_packets(&self.observer_player, auto_obs).await;
            self.observer_player.process_inbound_packets();
        }
        let mut decoded_observer: Vec<RecordedClientPacket> = raw_obs
            .into_iter()
            .filter_map(|(id, payload)| self.normalizer_observer.decode_packet(id, &payload))
            .collect();
        canonicalize_tick_packets(&mut decoded_observer);

        let snapshot = self.capture_snapshot(watched_offsets);
        Ok((decoded_primary, decoded_observer, snapshot))
    }

    #[must_use]
    #[expect(
        clippy::too_many_lines,
        reason = "Captures world blocks, player state, inventory, and equipment snapshots"
    )]
    pub fn capture_snapshot(&self, watched_offsets: &[[i32; 3]]) -> LockstepWorldSnapshot {
        let entity = &self.primary_player.living_entity.entity;
        let pos = entity.pos.load();
        let rel_pos = [
            round3(pos.x - f64::from(self.origin.0.x)),
            round3(pos.y - f64::from(self.origin.0.y)),
            round3(pos.z - f64::from(self.origin.0.z)),
        ];
        let yaw = round3_f32(entity.yaw.load());
        let pitch = round3_f32(entity.pitch.load());
        let on_ground = entity.on_ground.load(Ordering::Relaxed);
        let sprinting = entity.sprinting.load(Ordering::Relaxed);
        let sneaking = entity.sneaking.load(Ordering::Relaxed);
        let flying = self
            .primary_player
            .abilities
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .flying;
        let gamemode = match self.primary_player.gamemode.load() {
            GameMode::Survival => "survival",
            GameMode::Creative => "creative",
            GameMode::Adventure => "adventure",
            GameMode::Spectator => "spectator",
        }
        .to_string();
        let selected_slot = self.primary_player.inventory.get_selected_slot();
        let health = round3_f32(self.primary_player.living_entity.health.load());
        let food = i32::from(self.primary_player.hunger_manager.level.load());

        let mut inventory = Vec::new();
        {
            let main_inv = self
                .primary_player
                .inventory
                .main_inventory
                .read()
                .unwrap_or_else(PoisonError::into_inner);
            for (slot_idx, stack) in main_inv.iter().enumerate() {
                if !stack.is_empty()
                    && let Some(rec) =
                        RecordedItemStack::from_id_and_count(stack.item.id, stack.item_count)
                {
                    inventory.push(RecordedSlotItem {
                        slot: slot_idx,
                        item: rec,
                    });
                }
            }
        }
        {
            let equip = self
                .primary_player
                .inventory
                .entity_equipment
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            for (idx, slot) in [
                EquipmentSlot::OFF_HAND,
                EquipmentSlot::FEET,
                EquipmentSlot::LEGS,
                EquipmentSlot::CHEST,
                EquipmentSlot::HEAD,
            ]
            .iter()
            .enumerate()
            {
                let stack = equip.get(slot);
                if !stack.is_empty()
                    && let Some(rec) =
                        RecordedItemStack::from_id_and_count(stack.item.id, stack.item_count)
                {
                    inventory.push(RecordedSlotItem {
                        slot: 100 + idx,
                        item: rec,
                    });
                }
            }
        }

        let mut watched_blocks = Vec::with_capacity(watched_offsets.len());
        for offset in watched_offsets {
            let block_pos = BlockPos::new(
                self.origin.0.x + offset[0],
                self.origin.0.y + offset[1],
                self.origin.0.z + offset[2],
            );
            let state_id = self.world.get_block_state_id(&block_pos);
            watched_blocks.push(RecordedBlockUpdate {
                pos: *offset,
                state: format_canonical_block_state(state_id),
            });
        }

        LockstepWorldSnapshot {
            player_pos: rel_pos,
            yaw,
            pitch,
            on_ground,
            sprinting,
            sneaking,
            flying,
            gamemode,
            selected_slot,
            health,
            food,
            inventory,
            watched_blocks,
        }
    }

    pub async fn shutdown(self) {
        self.cancel.cancel();
        for player in self.server.get_all_players() {
            player.remove().await;
            self.server.remove_player(&player);
        }
        self.tasks.close();
        self.tasks.wait().await;
        self.server.shutdown().await;
    }
}

fn register_forced_arena_chunks(world: &World, base_cx: i32, base_cz: i32) {
    let mut forced = world
        .forced_chunks
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let mut chunk_loading = world
        .level
        .chunk_loading
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    for cx in (base_cx - 1)..=(base_cx + 1) {
        for cz in (base_cz - 1)..=(base_cz + 1) {
            let chunk = Vector2::new(cx, cz);
            forced.insert(chunk);
            chunk_loading.add_force_ticket(chunk);
        }
    }
    chunk_loading.send_change();
}

async fn wait_for_player(world: &Arc<World>, name: &str) -> Result<Arc<Player>, String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(player) = world.get_player_by_name(name) {
            return Ok(player);
        }
        if Instant::now() >= deadline {
            return Err(format!("Timed out waiting for player {name} in world"));
        }
        tokio::task::yield_now().await;
    }
}

async fn wait_for_inbound_packets(player: &Arc<Player>, expected_min: usize) {
    let deadline = Instant::now() + Duration::from_millis(500);
    while player.inbound_packets.len() < expected_min && Instant::now() < deadline {
        tokio::task::yield_now().await;
    }
}

/// Manages an external Vanilla Java Edition server process or remote TCP address
/// for live Dual-Server Lockstep runs (`--lockstep-java-jar` or `--lockstep-java-addr`).
pub struct ExternalJavaServerProcess {
    pub addr: SocketAddr,
    _child: Option<Child>,
    pub stdin: Option<ChildStdin>,
    pub stdout_rx: Option<UnboundedReceiver<String>>,
    pub temp_dir_path: Option<PathBuf>,
    _temp_dir: Option<TempDir>,
}

impl ExternalJavaServerProcess {
    #[expect(
        clippy::too_many_lines,
        reason = "Configures and spawns external Vanilla Java server subprocess"
    )]
    pub async fn connect_or_spawn(
        java_jar: Option<&Path>,
        java_addr: Option<SocketAddr>,
        world_save: Option<&Path>,
    ) -> Result<Self, String> {
        if let Some(addr) = java_addr {
            return Ok(Self {
                addr,
                _child: None,
                stdin: None,
                stdout_rx: None,
                temp_dir_path: None,
                _temp_dir: None,
            });
        }
        let jar_path =
            java_jar.ok_or_else(|| "No Java server jar or addr specified".to_string())?;
        if !jar_path.exists() {
            return Err(format!(
                "Java server jar does not exist at {}",
                jar_path.display()
            ));
        }

        let temp_dir = tempfile::tempdir()
            .map_err(|e| format!("Failed to create temp dir for Java server: {e}"))?;
        let temp_dir_path = temp_dir.path().to_path_buf();
        let port = pick_free_port()?;
        let addr = SocketAddr::from(([127, 0, 0, 1], port));

        if let Some(src_world) = world_save {
            copy_dir_recursive(src_world, &temp_dir_path.join("world"))?;
        }

        // Pre-install all `.nbt` structures from `assets/tests/lockstep_structures/` into
        // `<temp_dir>/world/generated/lockstep/structure/` so `place template lockstep:<name>` resolves.
        let struct_src_dir = default_lockstep_structures_dir();
        if struct_src_dir.is_dir() {
            let dst_struct_dir = temp_dir_path
                .join("world")
                .join("generated")
                .join("lockstep")
                .join("structure");
            copy_dir_recursive(&struct_src_dir, &dst_struct_dir)?;
        }

        std::fs::write(temp_dir_path.join("eula.txt"), "eula=true\n")
            .map_err(|e| format!("Failed to write eula.txt: {e}"))?;

        let ops_json = format!(
            "[\n  {{\"uuid\":\"{PRIMARY_BOT_UUID}\",\"name\":\"LockBot1\",\"level\":4,\"bypassesPlayerLimit\":true}},\n  {{\"uuid\":\"{OBSERVER_BOT_UUID}\",\"name\":\"LockBot2\",\"level\":4,\"bypassesPlayerLimit\":true}}\n]\n"
        );
        std::fs::write(temp_dir_path.join("ops.json"), ops_json)
            .map_err(|e| format!("Failed to write ops.json: {e}"))?;

        let server_props = format!(
            "server-ip=127.0.0.1\n\
             server-port={port}\n\
             online-mode=false\n\
             white-list=false\n\
             enforce-whitelist=false\n\
             enforce-secure-profile=false\n\
             network-compression-threshold=-1\n\
             spawn-protection=0\n\
             level-type=minecraft\\:flat\n\
             level-seed=0\n\
             gamemode=survival\n\
             difficulty=normal\n\
             view-distance=6\n\
             simulation-distance=6\n\
             pause-when-empty-seconds=0\n\
             enable-command-block=true\n\
             max-tick-time=-1\n"
        );
        std::fs::write(temp_dir_path.join("server.properties"), server_props)
            .map_err(|e| format!("Failed to write server.properties: {e}"))?;

        let java_bin = resolve_java_binary();
        let abs_jar = std::fs::canonicalize(jar_path)
            .map_err(|e| format!("Failed to canonicalize jar path: {e}"))?;

        let mut child = Command::new(&java_bin)
            .current_dir(&temp_dir_path)
            .arg("-Xmx2G")
            .arg("-Xms1G")
            .arg("-jar")
            .arg(&abs_jar)
            .arg("nogui")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| {
                format!(
                    "Failed to spawn Java server with {}: {e}",
                    java_bin.display()
                )
            })?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "Failed to capture Java server stdout".to_string())?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "Failed to capture Java server stdin".to_string())?;

        // Wait for "Done (" in stdout
        let mut reader = BufReader::new(stdout).lines();
        let deadline = Instant::now() + Duration::from_secs(90);
        let mut ready = false;
        while Instant::now() < deadline {
            if let Ok(Ok(Some(line))) =
                tokio::time::timeout(Duration::from_secs(2), reader.next_line()).await
                && line.contains("Done (")
            {
                ready = true;
                break;
            }
        }
        if !ready {
            return Err("Timed out waiting for external Java server to finish booting".to_string());
        }

        let (stdout_tx, stdout_rx) = unbounded_channel();
        tokio::spawn(async move {
            let mut r = reader;
            while let Ok(Some(line)) = r.next_line().await {
                let _ = stdout_tx.send(line);
            }
        });

        let mut proc = Self {
            addr,
            _child: Some(child),
            stdin: Some(stdin),
            stdout_rx: Some(stdout_rx),
            temp_dir_path: Some(temp_dir_path),
            _temp_dir: Some(temp_dir),
        };

        // Configure deterministic gamerules and 1000 TPS rate, then synchronize on console barrier
        for cmd in [
            "whitelist off",
            "tick rate 1000",
            "gamerule advance_time false",
            "gamerule advance_weather false",
            "gamerule spawn_mobs false",
            "gamerule random_tick_speed 0",
            "gamerule natural_health_regeneration false",
            "gamerule send_command_feedback false",
        ] {
            proc.send_console_command(cmd).await?;
        }
        proc.wait_for_console_barrier().await?;

        Ok(proc)
    }

    pub async fn send_console_command(&mut self, cmd: &str) -> Result<(), String> {
        if let Some(stdin) = &mut self.stdin {
            stdin
                .write_all(format!("{cmd}\n").as_bytes())
                .await
                .map_err(|e| format!("Failed to send console command '{cmd}': {e}"))?;
            stdin
                .flush()
                .await
                .map_err(|e| format!("Failed to flush console command '{cmd}': {e}"))?;
        }
        Ok(())
    }

    /// Sends `"seed\n"` to `stdin` and waits until `DedicatedServer.handleConsoleInputs()` on the
    /// main server thread executes it and logs `"Seed: ["` to `stdout`.
    /// Because `SeedCommand` passes `broadcastToAdmins = false`, zero packets are sent to players.
    pub async fn wait_for_console_barrier(&mut self) -> Result<(), String> {
        if self.stdin.is_none() || self.stdout_rx.is_none() {
            return Ok(());
        }
        self.send_console_command("seed").await?;
        let Some(rx) = &mut self.stdout_rx else {
            return Ok(());
        };
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let rem = deadline.saturating_duration_since(Instant::now());
            if rem.is_zero() {
                return Err("Timed out waiting for Java server console tick barrier".to_string());
            }
            match tokio::time::timeout(rem, rx.recv()).await {
                Ok(Some(line)) => {
                    if line.contains("Seed: [") {
                        return Ok(());
                    }
                }
                Ok(None) => {
                    return Err(
                        "Java server stdout closed while waiting for console barrier".to_string(),
                    );
                }
                Err(_) => {
                    return Err(
                        "Timed out waiting for Java server console tick barrier".to_string()
                    );
                }
            }
        }
    }

    /// Deterministically steps a frozen Java server by 1 tick and waits until that tick has
    /// completed `processQueuedPackets()`, `ServerLevel.tick()`, `sendNextChunks()`, and
    /// `player.connection.resumeFlushing()`.
    pub async fn step_and_wait_tick(&mut self) -> Result<(), String> {
        self.send_console_command("tick step 1").await?;
        // Barrier 1: executes in tick T alongside `tick step 1` (sets frozenTicksToRun = 1).
        self.wait_for_console_barrier().await?;
        // Barrier 2: executes in tick T + 1 (after tick T + 1's processQueuedPackets & ServerLevel.tick).
        self.wait_for_console_barrier().await?;
        // Barrier 3: executes in tick T + 2 (after tick T + 1's sendNextChunks & resumeFlushing).
        self.wait_for_console_barrier().await?;
        Ok(())
    }

    /// Ensures a structure `.nbt` file is installed in `<temp_dir>/world/generated/<ns>/structure/<path>.nbt`
    /// and returns its canonical `<ns>:<path>` identifier for `/place template`.
    pub fn ensure_structure_installed(&self, structure_id: &str) -> Result<String, String> {
        let src_path = resolve_structure_nbt_path(structure_id)?;
        let (ns, raw_path) = structure_id
            .split_once(':')
            .unwrap_or(("lockstep", structure_id));
        let clean_path = raw_path.strip_suffix(".nbt").unwrap_or(raw_path);
        if let Some(root) = &self.temp_dir_path {
            let dst = root
                .join("world")
                .join("generated")
                .join(ns)
                .join("structure")
                .join(format!("{clean_path}.nbt"));
            if !dst.exists() {
                if let Some(parent) = dst.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| {
                        format!("Failed to create structure dir {}: {e}", parent.display())
                    })?;
                }
                std::fs::copy(&src_path, &dst).map_err(|e| {
                    format!(
                        "Failed to copy structure {} to {}: {e}",
                        src_path.display(),
                        dst.display()
                    )
                })?;
            }
        }
        Ok(format!("{ns}:{clean_path}"))
    }
}

fn pick_free_port() -> Result<u16, String> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .map_err(|e| format!("Failed to bind ephemeral port: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("Failed to get local_addr: {e}"))?
        .port();
    Ok(port)
}

fn resolve_java_binary() -> PathBuf {
    let args: Vec<String> = std::env::args().collect();
    for window in args.windows(2) {
        if (window[0] == "--lockstep-java-bin" || window[0] == "--parity-java-bin")
            && !window[1].trim().is_empty()
        {
            return PathBuf::from(&window[1]);
        }
    }
    for env_var in ["PUMPKIN_LOCKSTEP_JAVA_BIN", "PUMPKIN_PARITY_JAVA_BIN"] {
        if let Ok(val) = std::env::var(env_var)
            && !val.trim().is_empty()
        {
            return PathBuf::from(val);
        }
    }
    PathBuf::from("java")
}

/// A live client-session peer connected to an external Vanilla Java Edition server.
pub struct ExternalJavaPeer {
    pub origin: BlockPos,
    pub primary_bot: LockstepBot,
    pub observer_bot: LockstepBot,
    pub normalizer_primary: PacketNormalizer,
    pub normalizer_observer: PacketNormalizer,
}

impl ExternalJavaPeer {
    pub async fn boot(
        java_proc: &mut ExternalJavaServerProcess,
        origin: BlockPos,
    ) -> Result<Self, String> {
        let ox = origin.0.x;
        let oy = origin.0.y;
        let oz = origin.0.z;
        java_proc
            .send_console_command(&format!(
                "forceload add {} {} {} {}",
                ox - 16,
                oz - 16,
                ox + 16,
                oz + 16
            ))
            .await?;
        java_proc.wait_for_console_barrier().await?;

        for cmd in [
            format!(
                "fill {} {} {} {} {} {} minecraft:stone strict",
                ox - 4,
                oy - 1,
                oz - 4,
                ox + 4,
                oy - 1,
                oz + 4
            ),
            format!(
                "setblock {ox} {} {} minecraft:stone strict",
                oy - 1,
                oz + 10
            ),
            format!("setworldspawn {ox} {oy} {oz}"),
        ] {
            java_proc.send_console_command(&cmd).await?;
        }
        java_proc.wait_for_console_barrier().await?;

        let mut primary_bot =
            LockstepBot::connect_and_login(java_proc.addr, "LockBot1", PRIMARY_BOT_UUID).await?;
        let mut observer_bot =
            LockstepBot::connect_and_login(java_proc.addr, "LockBot2", OBSERVER_BOT_UUID).await?;

        for cmd in [
            "op LockBot1".to_string(),
            "op LockBot2".to_string(),
            "tick rate 1000".to_string(),
            format!("tp LockBot1 {ox}.5 {oy}.0 {oz}.5 0 0"),
            format!("tp LockBot2 {ox}.5 {oy}.0 {}.5 180 0", oz + 10),
        ] {
            java_proc.send_console_command(&cmd).await?;
        }
        for _ in 0..6 {
            java_proc.wait_for_console_barrier().await?;
            let _ = primary_bot.drain_until_pong(Duration::from_secs(5)).await?;
            let _ = observer_bot
                .drain_until_pong(Duration::from_secs(5))
                .await?;
            primary_bot
                .execute_action(&LockstepAction::ClientTickEnd, origin, None)
                .await?;
            observer_bot
                .execute_action(&LockstepAction::ClientTickEnd, origin, None)
                .await?;
        }

        java_proc.send_console_command("tick freeze").await?;
        java_proc.wait_for_console_barrier().await?;
        java_proc.wait_for_console_barrier().await?;
        let _ = primary_bot.drain_until_pong(Duration::from_secs(5)).await?;
        let _ = observer_bot
            .drain_until_pong(Duration::from_secs(5))
            .await?;

        let normalizer_primary = PacketNormalizer::new(
            origin,
            primary_bot.entity_id,
            Some(observer_bot.entity_id),
            false,
        );
        let normalizer_observer = PacketNormalizer::new(
            origin,
            observer_bot.entity_id,
            Some(primary_bot.entity_id),
            false,
        );

        Ok(Self {
            origin,
            primary_bot,
            observer_bot,
            normalizer_primary,
            normalizer_observer,
        })
    }

    #[expect(
        clippy::too_many_lines,
        reason = "Resets arena blocks, places structures, and resets player state on external Java server"
    )]
    pub async fn reset_for_scenario(
        &mut self,
        java_proc: &mut ExternalJavaServerProcess,
        scenario: &LockstepScenario,
    ) -> Result<(), String> {
        let ox = self.origin.0.x;
        let oy = self.origin.0.y;
        let oz = self.origin.0.z;
        let gm_name = match scenario.initial_gamemode {
            GameMode::Survival => "survival",
            GameMode::Creative => "creative",
            GameMode::Adventure => "adventure",
            GameMode::Spectator => "spectator",
        };
        let (obs_x, obs_y, obs_z) = if scenario.spawn_observer {
            (
                f64::from(ox) + scenario.observer_offset[0],
                f64::from(oy) + scenario.observer_offset[1],
                f64::from(oz) + scenario.observer_offset[2],
            )
        } else {
            (f64::from(ox) + 0.5, f64::from(oy), f64::from(oz) + 10.5)
        };

        self.primary_bot
            .execute_action(&LockstepAction::SetHeldItem { slot: 0 }, self.origin, None)
            .await?;
        self.primary_bot
            .execute_action(&LockstepAction::PlayerInput { flags: 0 }, self.origin, None)
            .await?;
        self.primary_bot
            .execute_action(
                &LockstepAction::PlayerCommand {
                    command: super::client::PlayerCommandKind::StopSprinting,
                    jump_boost: 0,
                },
                self.origin,
                None,
            )
            .await?;
        self.primary_bot
            .execute_action(&LockstepAction::ClientTickEnd, self.origin, None)
            .await?;
        self.observer_bot
            .execute_action(&LockstepAction::ClientTickEnd, self.origin, None)
            .await?;
        // Ensure Netty IO thread has queued all reset packets before stepping
        let _ = self
            .primary_bot
            .drain_until_pong(Duration::from_secs(5))
            .await?;
        let _ = self
            .observer_bot
            .drain_until_pong(Duration::from_secs(5))
            .await?;

        let mut setup_cmds = vec!["tick rate 1000".to_string(), "tick freeze".to_string()];

        if scenario.world_save.is_none() {
            setup_cmds.extend([
                format!(
                    "fill {} {} {} {} {} {} minecraft:air strict",
                    ox - 4,
                    oy,
                    oz - 4,
                    ox + 4,
                    oy + 5,
                    oz + 4
                ),
                format!(
                    "fill {} {} {} {} {} {} minecraft:stone strict",
                    ox - 4,
                    oy - 1,
                    oz - 4,
                    ox + 4,
                    oy - 1,
                    oz + 4
                ),
                format!(
                    "setblock {ox} {} {} minecraft:stone strict",
                    oy - 1,
                    oz + 10
                ),
                "kill @e[type=!minecraft:player]".to_string(),
            ]);
        }

        setup_cmds.extend([
            "clear LockBot1".to_string(),
            "clear LockBot2".to_string(),
            "effect give LockBot1 minecraft:instant_health 1 255 true".to_string(),
            "effect give LockBot2 minecraft:instant_health 1 255 true".to_string(),
            "effect give LockBot1 minecraft:saturation 1 255 true".to_string(),
            "effect give LockBot2 minecraft:saturation 1 255 true".to_string(),
            "xp add LockBot1 -100000 levels".to_string(),
            format!("gamemode {gm_name} LockBot1"),
            "gamemode survival LockBot2".to_string(),
            format!("tp LockBot1 {ox}.5 {oy}.0 {oz}.5 0 0"),
            format!("tp LockBot2 {obs_x} {obs_y} {obs_z} 180 0"),
        ]);

        if let Some(structure_id) = &scenario.structure {
            let template_id = java_proc.ensure_structure_installed(structure_id)?;
            let sx = ox + scenario.structure_offset[0];
            let sy = oy + scenario.structure_offset[1];
            let sz = oz + scenario.structure_offset[2];
            setup_cmds.push(format!(
                "place template {template_id} {sx} {sy} {sz} none none 1.0 0 strict"
            ));
        }

        for placement in &scenario.initial_blocks {
            let bx = ox + placement.offset[0];
            let by = oy + placement.offset[1];
            let bz = oz + placement.offset[2];
            let canonical = format_canonical_block_state(placement.state_id);
            setup_cmds.push(format!("setblock {bx} {by} {bz} {canonical} strict"));
        }

        for item in &scenario.initial_inventory {
            let slot_name = if item.slot < 9 {
                format!("hotbar.{}", item.slot)
            } else {
                format!("inventory.{}", item.slot - 9)
            };
            setup_cmds.push(format!(
                "item replace entity LockBot1 {slot_name} with {} {}",
                item.item, item.count
            ));
        }

        setup_cmds.push("kill @e[type=minecraft:item]".to_string());

        for cmd in &setup_cmds {
            if java_proc.stdin.is_some() {
                java_proc.send_console_command(cmd).await?;
            } else {
                self.primary_bot
                    .execute_action(
                        &LockstepAction::ChatCommand {
                            command: cmd.clone(),
                        },
                        self.origin,
                        None,
                    )
                    .await?;
            }
        }

        for _ in 0..2 {
            java_proc.step_and_wait_tick().await?;
            let _ = self
                .primary_bot
                .drain_until_pong(Duration::from_secs(5))
                .await?;
            let _ = self
                .observer_bot
                .drain_until_pong(Duration::from_secs(5))
                .await?;
        }
        self.primary_bot
            .execute_action(&LockstepAction::ClientTickEnd, self.origin, None)
            .await?;
        self.observer_bot
            .execute_action(&LockstepAction::ClientTickEnd, self.origin, None)
            .await?;
        let _ = self
            .primary_bot
            .drain_until_pong(Duration::from_secs(5))
            .await?;
        let _ = self
            .observer_bot
            .drain_until_pong(Duration::from_secs(5))
            .await?;
        self.primary_bot.spawned_entity_ids.clear();
        self.observer_bot.spawned_entity_ids.clear();

        let mut post_cmds = vec![
            "effect clear LockBot1".to_string(),
            "effect clear LockBot2".to_string(),
        ];
        for mob in &scenario.initial_mobs {
            let mx = f64::from(ox) + mob.offset[0];
            let my = f64::from(oy) + mob.offset[1];
            let mz = f64::from(oz) + mob.offset[2];
            post_cmds.push(format!(
                "summon {} {mx} {my} {mz} {{NoAI:1b,PersistenceRequired:1b}}",
                mob.entity_type
            ));
        }

        for cmd in &post_cmds {
            if java_proc.stdin.is_some() {
                java_proc.send_console_command(cmd).await?;
            } else {
                self.primary_bot
                    .execute_action(
                        &LockstepAction::ChatCommand {
                            command: cmd.clone(),
                        },
                        self.origin,
                        None,
                    )
                    .await?;
            }
        }

        for _ in 0..2 {
            java_proc.step_and_wait_tick().await?;
            let _ = self
                .primary_bot
                .drain_until_pong(Duration::from_secs(5))
                .await?;
            let _ = self
                .observer_bot
                .drain_until_pong(Duration::from_secs(5))
                .await?;
        }
        for _ in 0..5 {
            if self.primary_bot.spawned_entity_ids.len() >= scenario.initial_mobs.len() {
                break;
            }
            java_proc.step_and_wait_tick().await?;
            let _ = self
                .primary_bot
                .drain_until_pong(Duration::from_secs(5))
                .await?;
            let _ = self
                .observer_bot
                .drain_until_pong(Duration::from_secs(5))
                .await?;
        }
        self.primary_bot
            .execute_action(&LockstepAction::ClientTickEnd, self.origin, None)
            .await?;
        self.observer_bot
            .execute_action(&LockstepAction::ClientTickEnd, self.origin, None)
            .await?;
        if java_proc.stdin.is_some() {
            java_proc.send_console_command("tick rate 20").await?;
            java_proc.send_console_command("tick sprint 1").await?;
            java_proc.wait_for_console_barrier().await?;
            java_proc.send_console_command("tick freeze").await?;
            java_proc.wait_for_console_barrier().await?;
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        let _ = self
            .primary_bot
            .drain_until_pong(Duration::from_secs(5))
            .await?;
        let _ = self
            .observer_bot
            .drain_until_pong(Duration::from_secs(5))
            .await?;
        self.primary_bot.sequence_counter = 0;
        self.primary_bot.active_window_id = None;
        self.observer_bot.sequence_counter = 0;
        self.observer_bot.active_window_id = None;

        self.normalizer_primary = PacketNormalizer::new(
            self.origin,
            self.primary_bot.entity_id,
            Some(self.observer_bot.entity_id),
            scenario.include_sounds,
        );
        self.normalizer_observer = PacketNormalizer::new(
            self.origin,
            self.observer_bot.entity_id,
            Some(self.primary_bot.entity_id),
            scenario.include_sounds,
        );

        for (idx, &mob_id) in self.primary_bot.spawned_entity_ids.iter().enumerate() {
            self.normalizer_primary
                .register_entity_label(mob_id, format!("mob_{idx}"));
            self.normalizer_observer
                .register_entity_label(mob_id, format!("mob_{idx}"));
        }

        Ok(())
    }

    pub async fn step_tick(
        &mut self,
        java_proc: &mut ExternalJavaServerProcess,
        actions: &[(BotTarget, LockstepAction)],
    ) -> Result<(Vec<RecordedClientPacket>, Vec<RecordedClientPacket>), String> {
        let obs_eid = Some(self.observer_bot.entity_id);
        let prim_eid = Some(self.primary_bot.entity_id);

        let action_has_tick_step = actions.iter().any(|(_, a)| {
            matches!(
                a,
                LockstepAction::ChatCommand { command } if command.trim().starts_with("tick step")
            )
        });
        let action_has_tick_step_1 = actions.iter().any(|(_, a)| {
            matches!(
                a,
                LockstepAction::ChatCommand { command } if command.trim() == "tick step 1"
            )
        });

        let mut primary_sent_tick_end = false;
        let mut observer_sent_tick_end = false;
        for (target, action) in actions {
            match target {
                BotTarget::Primary => {
                    if matches!(action, LockstepAction::ClientTickEnd) {
                        primary_sent_tick_end = true;
                    }
                    self.primary_bot
                        .execute_action(action, self.origin, obs_eid)
                        .await?;
                }
                BotTarget::Observer => {
                    if matches!(action, LockstepAction::ClientTickEnd) {
                        observer_sent_tick_end = true;
                    }
                    self.observer_bot
                        .execute_action(action, self.origin, prim_eid)
                        .await?;
                }
            }
        }
        if !primary_sent_tick_end {
            self.primary_bot
                .execute_action(&LockstepAction::ClientTickEnd, self.origin, obs_eid)
                .await?;
        }
        if !observer_sent_tick_end {
            self.observer_bot
                .execute_action(&LockstepAction::ClientTickEnd, self.origin, prim_eid)
                .await?;
        }
        if !action_has_tick_step {
            self.observer_bot
                .execute_action(
                    &LockstepAction::ChatCommand {
                        command: "tick step 1".to_string(),
                    },
                    self.origin,
                    prim_eid,
                )
                .await?;
        }

        // Barrier 0 (Netty IO barrier): SPlayPingRequest is handled on the Netty IO thread in
        // ServerGamePacketListenerImpl, so receiving CPingResponse proves all preceding client
        // packets for this tick have been read off the socket and queued in `packetProcessor`,
        // and `SChatCommand("tick step 1")` has been submitted to `MinecraftServer.execute()`
        // inside `waitUntilNextTick()` before the next tick begins.
        let mut pre_primary = self
            .primary_bot
            .drain_until_pong(Duration::from_secs(5))
            .await?;
        let mut pre_obs = self
            .observer_bot
            .drain_until_pong(Duration::from_secs(5))
            .await?;

        if java_proc.stdin.is_some() {
            java_proc.wait_for_console_barrier().await?;
            tokio::time::sleep(Duration::from_millis(2)).await;
        }

        let post_primary = self
            .primary_bot
            .drain_until_pong(Duration::from_secs(5))
            .await?;
        pre_primary.extend(post_primary);
        let mut decoded_primary: Vec<RecordedClientPacket> = pre_primary
            .into_iter()
            .filter_map(|(id, payload)| self.normalizer_primary.decode_packet(id, &payload))
            .filter(|pkt| {
                action_has_tick_step_1
                    || !matches!(pkt, RecordedClientPacket::TickingStep { tick_steps: 1 })
            })
            .collect();
        canonicalize_tick_packets(&mut decoded_primary);

        let post_obs = self
            .observer_bot
            .drain_until_pong(Duration::from_secs(5))
            .await?;
        pre_obs.extend(post_obs);
        let mut decoded_observer: Vec<RecordedClientPacket> = pre_obs
            .into_iter()
            .filter_map(|(id, payload)| self.normalizer_observer.decode_packet(id, &payload))
            .filter(|pkt| {
                action_has_tick_step_1
                    || !matches!(pkt, RecordedClientPacket::TickingStep { tick_steps: 1 })
            })
            .collect();
        canonicalize_tick_packets(&mut decoded_observer);

        Ok((decoded_primary, decoded_observer))
    }
}
