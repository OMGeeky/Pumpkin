use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use pumpkin_core::{
    block::{
        blocks::redstone::{buttons::ButtonBlock, lever::LeverBlock},
        entities::{block_entity_from_nbt, create_block_entity, has_block_block_entity},
    },
    server::Server,
    world::{BlockFlags, World},
};
use pumpkin_data::{
    Block, BlockDirection, BlockState,
    block_properties::{
        BlockProperties, ComparatorLikeProperties, DaylightDetectorLikeProperties,
        FenceGateProperties, LeverLikeProperties, ModeComparator, NoteBlockLikeProperties,
        OakDoorLikeProperties, OakTrapdoorLikeProperties, RepeaterLikeProperties,
    },
    dimension::Dimension,
    packet::{CURRENT_MC_VERSION, clientbound::play as c_play},
    sound::{Sound, SoundCategory},
};
use pumpkin_gametest::{
    GameTestRunner, GameTestStructureBlock, GameTestStructureTemplate, NbtCompound,
    parse_canonical_block_state,
};
use pumpkin_protocol::{
    PositionFlag, ServerPacket, VarInt,
    codec::var_long::VarLong,
    java::{
        client::play::{CKeepAlive, CPlayerPosition},
        server::play::{
            SChunkBatch, SConfirmTeleport, SKeepAlive, SPlayPingRequest, SSetStructureBlock,
            SUseItemOn,
        },
    },
    ser::NetworkReadExt,
};
use pumpkin_util::{
    Hand,
    math::{position::BlockPos, vector2::Vector2, vector3::Vector3},
};
use serde::{Deserialize, Serialize};
use tempfile::TempDir;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::mpsc::{UnboundedReceiver, unbounded_channel},
    time::Instant,
};

use crate::bot::HeadlessBot;

const PARITY_ORIGIN: BlockPos = BlockPos::new(160, 200, 160);

/// Declarative `test.json` manifest inside a contraption test directory.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContraptionTestManifest {
    pub start: String,
    #[serde(default)]
    pub actions: Vec<ContraptionAction>,
    pub checks: Vec<ContraptionCheck>,
}

/// Action executed at a specific tick (relative to structure placement at tick 0).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContraptionAction {
    #[serde(default)]
    pub tick: u32,
    #[serde(flatten)]
    pub kind: ContraptionActionKind,
}

/// Supported contraption actions at structure-relative `[x, y, z]` coordinates.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContraptionActionKind {
    UseBlock { pos: [i32; 3] },
    PressButton { pos: [i32; 3] },
    PullLever { pos: [i32; 3] },
    SetBlock { pos: [i32; 3], block: String },
    BreakBlock { pos: [i32; 3] },
}

/// Expected `.nbt` structure checkpoint at a specific tick.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContraptionCheck {
    pub tick: u32,
    pub nbt: String,
}

/// Runs all `.nbt` contraption parity tests discovered under `assets/tests/redstone_parity/`.
///
/// Pass `--record-parity` (optionally with `--parity-java-jar <jar>`) to record or update
/// the checkpoint `.nbt` files for each contraption.
///
/// # Errors
///
/// Returns an error if a contraption file is invalid or if a checkpoint diverges in strict mode.
#[expect(
    clippy::too_many_lines,
    reason = "Runs contraption discovery, optional Java recording, and checkpoint evaluation"
)]
pub async fn run_redstone_parity_suite(
    server: &Arc<Server>,
    _runner: &mut GameTestRunner,
    summary_lines: &mut Vec<String>,
) -> Result<(), String> {
    let suite_dir = resolve_parity_dir()
        .ok_or_else(|| "Could not locate assets/tests/redstone_parity directory".to_string())?;
    let record_mode = cli_has_flag("--record-parity")
        || std::env::var("PUMPKIN_PARITY_RECORD")
            .is_ok_and(|val| matches!(val.as_str(), "1" | "true" | "TRUE"));
    let strict_mode = cli_has_flag("--strict-parity")
        || std::env::var("PUMPKIN_PARITY_STRICT")
            .is_ok_and(|val| matches!(val.as_str(), "1" | "true" | "TRUE"));

    let contraption_dirs = discover_contraption_dirs(&suite_dir)?;
    if contraption_dirs.is_empty() {
        return Err(format!(
            "No contraption folders containing test.json found in {}",
            suite_dir.display()
        ));
    }

    if record_mode
        && let Some(java_jar) = resolve_option_path("--parity-java-jar", "PUMPKIN_PARITY_JAVA_JAR")
    {
        record_contraptions_on_java(&java_jar, &contraption_dirs).await?;
        summary_lines.push(format!(
            "[PASS] redstone_parity_java_record (recorded {} contraptions from Vanilla Java)",
            contraption_dirs.len()
        ));
    }

    let world = server.get_world_from_dimension(&Dimension::OVERWORLD);
    let base_cx = PARITY_ORIGIN.0.x >> 4;
    let base_cz = PARITY_ORIGIN.0.z >> 4;
    force_load_parity_chunks(&world, base_cx - 1, base_cx + 2, base_cz - 1, base_cz + 2);
    for cx in (base_cx - 1)..=(base_cx + 2) {
        for cz in (base_cz - 1)..=(base_cz + 2) {
            world
                .level
                .get_or_fetch_chunk(Vector2::new(cx, cz), |_| ())
                .await;
        }
    }
    world.update_active_chunks();

    let mut passed_contraptions = 0usize;
    let mut total_checks = 0usize;
    let mut passed_checks = 0usize;
    let mut failures = Vec::new();

    for dir in &contraption_dirs {
        let name = dir
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown");
        let manifest_path = dir.join("test.json");
        let raw_manifest = fs::read_to_string(&manifest_path)
            .map_err(|e| format!("Failed to read {}: {e}", manifest_path.display()))?;
        let manifest: ContraptionTestManifest = serde_json::from_str(&raw_manifest)
            .map_err(|e| format!("Failed to parse {}: {e}", manifest_path.display()))?;

        let start_path = dir.join(&manifest.start);
        let start_bytes = fs::read(&start_path)
            .map_err(|e| format!("Failed to read {}: {e}", start_path.display()))?;
        let start_template = GameTestStructureTemplate::from_gzip_bytes(&start_bytes)
            .map_err(|e| format!("Failed to parse {}: {e}", start_path.display()))?;

        place_contraption_in_world(&world, PARITY_ORIGIN, &start_template);

        let max_tick = manifest
            .actions
            .iter()
            .map(|a| a.tick)
            .chain(manifest.checks.iter().map(|c| c.tick))
            .max()
            .unwrap_or(0);

        let mut contraption_ok = true;
        for tick in 0..=max_tick {
            if tick > 0 {
                server.tick();
            }

            for action in manifest.actions.iter().filter(|a| a.tick == tick) {
                execute_contraption_action(&world, PARITY_ORIGIN, &action.kind)?;
            }

            for check in manifest.checks.iter().filter(|c| c.tick == tick) {
                total_checks += 1;
                let actual =
                    capture_structure_from_world(&world, PARITY_ORIGIN, start_template.size())?;
                let check_path = dir.join(&check.nbt);

                if record_mode
                    && resolve_option_path("--parity-java-jar", "PUMPKIN_PARITY_JAVA_JAR").is_none()
                {
                    let bytes = actual.to_gzip_bytes().map_err(|e| {
                        format!("Failed to serialize {}: {e}", check_path.display())
                    })?;
                    fs::write(&check_path, bytes)
                        .map_err(|e| format!("Failed to write {}: {e}", check_path.display()))?;
                }

                let expected_bytes = fs::read(&check_path)
                    .map_err(|e| format!("Failed to read {}: {e}", check_path.display()))?;
                let expected = GameTestStructureTemplate::from_gzip_bytes(&expected_bytes)
                    .map_err(|e| format!("Failed to parse {}: {e}", check_path.display()))?;

                let mismatches = actual.compare(&expected);
                if mismatches.is_empty() {
                    passed_checks += 1;
                } else {
                    contraption_ok = false;
                    let preview: Vec<String> =
                        mismatches.iter().take(5).map(ToString::to_string).collect();
                    let detail = format!(
                        "[DIVERGED] {name} at tick {tick} ({}): {} mismatch(es): {}",
                        check.nbt,
                        mismatches.len(),
                        preview.join("; ")
                    );
                    summary_lines.push(format!("  {detail}"));
                    failures.push(detail);
                }
            }
        }

        if contraption_ok {
            passed_contraptions += 1;
            summary_lines.push(format!(
                "  [COMPLIANT] {name} ({}/{} NBT checkpoints matched)",
                manifest.checks.len(),
                manifest.checks.len()
            ));
        }
    }

    summary_lines.push(format!(
        "[PASS] redstone_parity ({}/{} contraptions compliant, {}/{} NBT checkpoints matched)",
        passed_contraptions,
        contraption_dirs.len(),
        passed_checks,
        total_checks
    ));

    if strict_mode && !failures.is_empty() {
        return Err(format!(
            "{} redstone parity checkpoint(s) diverged in strict mode",
            failures.len()
        ));
    }

    Ok(())
}

fn discover_contraption_dirs(suite_dir: &Path) -> Result<Vec<PathBuf>, String> {
    let entries = fs::read_dir(suite_dir)
        .map_err(|e| format!("Failed to read {}: {e}", suite_dir.display()))?;
    let mut dirs = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        if path.is_dir() && path.join("test.json").is_file() {
            dirs.push(path);
        }
    }
    dirs.sort();
    Ok(dirs)
}

fn place_contraption_in_world(
    world: &Arc<World>,
    origin: BlockPos,
    template: &GameTestStructureTemplate,
) {
    let size = template.size();
    let min_pos = BlockPos::new(origin.0.x - 2, origin.0.y - 2, origin.0.z - 2);
    let max_pos = BlockPos::new(
        origin.0.x + size[0] + 2,
        origin.0.y + size[1] + 2,
        origin.0.z + size[2] + 2,
    );

    let clear_flags = BlockFlags::FORCE_STATE
        | BlockFlags::SKIP_DROPS
        | BlockFlags::SKIP_REDSTONE_WIRE_STATE_REPLACEMENT
        | BlockFlags::SKIP_BLOCK_ADDED_CALLBACK
        | BlockFlags::SKIP_BLOCK_ENTITY_REPLACED_CALLBACK;

    let air_id = Block::AIR.default_state.id;

    for x in (origin.0.x - 1)..=(origin.0.x + size[0]) {
        for y in (origin.0.y - 1)..=(origin.0.y + size[1]) {
            for z in (origin.0.z - 1)..=(origin.0.z + size[2]) {
                let pos = BlockPos::new(x, y, z);
                world.remove_block_entity(&pos);
                let _ = world.set_block_state(&pos, air_id, clear_flags);
            }
        }
    }

    let place_flags = BlockFlags::NOTIFY_LISTENERS
        | BlockFlags::MOVED
        | BlockFlags::SKIP_REDSTONE_WIRE_STATE_REPLACEMENT
        | BlockFlags::SKIP_BLOCK_ADDED_CALLBACK;

    for block in template.blocks() {
        let pos = BlockPos::new(
            origin.0.x + block.position[0],
            origin.0.y + block.position[1],
            origin.0.z + block.position[2],
        );
        let _ = world.set_block_state(&pos, block.state, place_flags);
        if let Some(nbt) = &block.nbt {
            let mut full_nbt = nbt.clone();
            full_nbt.put_int("x", pos.0.x);
            full_nbt.put_int("y", pos.0.y);
            full_nbt.put_int("z", pos.0.z);
            if let Some(entity) = block_entity_from_nbt(&full_nbt) {
                world.add_block_entity(entity);
            }
        } else {
            let block_def = Block::from_state_id(block.state);
            if has_block_block_entity(block_def) {
                let state = BlockState::from_id(block.state);
                if let Some(entity) = create_block_entity(state.block_entity_type, pos) {
                    world.add_block_entity(entity);
                }
            }
        }
    }

    let min_cx = min_pos.0.x >> 4;
    let max_cx = max_pos.0.x >> 4;
    let min_cz = min_pos.0.z >> 4;
    let max_cz = max_pos.0.z >> 4;
    for cx in min_cx..=max_cx {
        for cz in min_cz..=max_cz {
            let chunk_pos = Vector2::new(cx, cz);
            if let Some(chunk) = world.level.loaded_chunks.get(&chunk_pos) {
                chunk.block_ticks.clear_area(&min_pos, &max_pos);
                chunk.fluid_ticks.clear_area(&min_pos, &max_pos);
                if !chunk.block_ticks.has_ticks() && !chunk.fluid_ticks.has_ticks() {
                    world.level.chunks_with_scheduled_ticks.remove(&chunk_pos);
                }
            }
        }
    }
    world.clear_synced_block_events_in_box(&min_pos, &max_pos);
}

fn capture_structure_from_world(
    world: &Arc<World>,
    origin: BlockPos,
    size: [i32; 3],
) -> Result<GameTestStructureTemplate, String> {
    let mut blocks = Vec::new();
    for ry in 0..size[1] {
        for rz in 0..size[2] {
            for rx in 0..size[0] {
                let pos = BlockPos::new(origin.0.x + rx, origin.0.y + ry, origin.0.z + rz);
                let state = world.get_block_state_id(&pos);
                let nbt = world.get_block_entity(&pos).map(|entity| {
                    let mut compound = NbtCompound::new();
                    entity.write_internal(&mut compound);
                    compound
                        .child_tags
                        .retain(|k, _| !matches!(k.as_ref(), "x" | "y" | "z"));
                    compound
                });
                blocks.push(GameTestStructureBlock {
                    position: [rx, ry, rz],
                    state,
                    nbt,
                    test_mode: None,
                });
            }
        }
    }
    GameTestStructureTemplate::new(size, blocks).map_err(|e| e.to_string())
}

fn execute_contraption_action(
    world: &Arc<World>,
    origin: BlockPos,
    action: &ContraptionActionKind,
) -> Result<(), String> {
    match action {
        ContraptionActionKind::PressButton { pos } => {
            let world_pos = BlockPos::new(
                origin.0.x + pos[0],
                origin.0.y + pos[1],
                origin.0.z + pos[2],
            );
            let block = world.get_block(&world_pos);
            if !LeverLikeProperties::handles_block_id(block.id) || block == &Block::LEVER {
                return Err(format!(
                    "Expected button at {pos:?}, found '{}'",
                    block.name
                ));
            }
            ButtonBlock::press(world, &world_pos);
            Ok(())
        }
        ContraptionActionKind::PullLever { pos } => {
            let world_pos = BlockPos::new(
                origin.0.x + pos[0],
                origin.0.y + pos[1],
                origin.0.z + pos[2],
            );
            let block = world.get_block(&world_pos);
            if block != &Block::LEVER {
                return Err(format!("Expected lever at {pos:?}, found '{}'", block.name));
            }
            LeverBlock::pull(world, &world_pos);
            Ok(())
        }
        ContraptionActionKind::UseBlock { pos } => {
            let world_pos = BlockPos::new(
                origin.0.x + pos[0],
                origin.0.y + pos[1],
                origin.0.z + pos[2],
            );
            use_block_at(world, &world_pos)
        }
        ContraptionActionKind::SetBlock { pos, block } => {
            let world_pos = BlockPos::new(
                origin.0.x + pos[0],
                origin.0.y + pos[1],
                origin.0.z + pos[2],
            );
            let state_id = parse_canonical_block_state(block).map_err(|e| e.to_string())?;
            world.remove_block_entity(&world_pos);
            let _ = world.set_block_state(&world_pos, state_id, BlockFlags::NOTIFY_ALL);
            let block_def = Block::from_state_id(state_id);
            if has_block_block_entity(block_def) {
                let state = BlockState::from_id(state_id);
                if let Some(entity) = create_block_entity(state.block_entity_type, world_pos) {
                    world.add_block_entity(entity);
                }
            }
            Ok(())
        }
        ContraptionActionKind::BreakBlock { pos } => {
            let world_pos = BlockPos::new(
                origin.0.x + pos[0],
                origin.0.y + pos[1],
                origin.0.z + pos[2],
            );
            world.remove_block_entity(&world_pos);
            let _ = world.set_block_state(
                &world_pos,
                Block::AIR.default_state.id,
                BlockFlags::NOTIFY_ALL,
            );
            Ok(())
        }
    }
}

fn use_block_at(world: &Arc<World>, pos: &BlockPos) -> Result<(), String> {
    let (block, state) = world.get_block_and_state(pos);
    if block == &Block::LEVER {
        LeverBlock::pull(world, pos);
        return Ok(());
    }
    if LeverLikeProperties::handles_block_id(block.id) {
        ButtonBlock::press(world, pos);
        return Ok(());
    }
    if block == &Block::REPEATER {
        let mut props = RepeaterLikeProperties::from_state_id(state.id);
        props.delay = if props.delay == 4 { 1 } else { props.delay + 1 };
        let _ = world.set_block_state(pos, props.to_state_id(block), BlockFlags::NOTIFY_ALL);
        return Ok(());
    }
    if block == &Block::COMPARATOR {
        let mut props = ComparatorLikeProperties::from_state_id(state.id);
        props.mode = match props.mode {
            ModeComparator::Compare => ModeComparator::Subtract,
            ModeComparator::Subtract => ModeComparator::Compare,
        };
        let new_state = props.to_state_id(block);
        let _ = world.set_block_state(pos, new_state, BlockFlags::NOTIFY_LISTENERS);
        if let Some(pumpkin_block) = world.block_registry.get_pumpkin_block(block.id) {
            pumpkin_block.on_scheduled_tick(pumpkin_core::block::OnScheduledTickArgs {
                world,
                block,
                position: pos,
            });
        }
        return Ok(());
    }
    if block == &Block::NOTE_BLOCK {
        let mut props = NoteBlockLikeProperties::from_state_id(state.id);
        props.note = (props.note + 1) % 25;
        let new_state = props.to_state_id(block);
        let _ = world.set_block_state(pos, new_state, BlockFlags::NOTIFY_ALL);
        world.add_synced_block_event(*pos, 0, 0);
        return Ok(());
    }
    if block == &Block::DAYLIGHT_DETECTOR {
        let mut props = DaylightDetectorLikeProperties::from_state_id(state.id);
        props.inverted = !props.inverted;
        let _ = world.set_block_state(pos, props.to_state_id(block), BlockFlags::NOTIFY_LISTENERS);
        world.update_neighbors(pos, None);
        return Ok(());
    }
    if OakDoorLikeProperties::handles_block_id(block.id) && block != &Block::IRON_DOOR {
        let mut props = OakDoorLikeProperties::from_state_id(state.id);
        props.open = !props.open;
        let _ = world.set_block_state(pos, props.to_state_id(block), BlockFlags::NOTIFY_ALL);
        return Ok(());
    }
    if OakTrapdoorLikeProperties::handles_block_id(block.id) && block != &Block::IRON_TRAPDOOR {
        let mut props = OakTrapdoorLikeProperties::from_state_id(state.id);
        props.open = !props.open;
        let _ = world.set_block_state(pos, props.to_state_id(block), BlockFlags::NOTIFY_ALL);
        return Ok(());
    }
    if FenceGateProperties::handles_block_id(block.id) {
        let mut props = FenceGateProperties::from_state_id(state.id);
        props.open = !props.open;
        let _ = world.set_block_state(pos, props.to_state_id(block), BlockFlags::NOTIFY_ALL);
        world.play_sound_fine(
            Sound::BlockFenceGateOpen,
            SoundCategory::Blocks,
            &pos.to_centered_f64(),
            1.0,
            1.0,
        );
        return Ok(());
    }
    Err(format!(
        "Unsupported use_block target '{}' at {pos}",
        block.name
    ))
}

async fn record_contraptions_on_java(
    java_jar: &Path,
    contraption_dirs: &[PathBuf],
) -> Result<(), String> {
    let mut java_proc = JavaRecorderServer::spawn(java_jar).await?;
    let temp_root = java_proc.temp_dir_path.clone();
    let res = record_contraptions_on_java_inner(&mut java_proc, &temp_root, contraption_dirs).await;
    if let Err(err) = res {
        let log_path = temp_root.join("logs").join("latest.log");
        let log_tail = fs::read_to_string(&log_path).unwrap_or_default();
        let all_lines: Vec<&str> = log_tail.lines().collect();
        let start = all_lines.len().saturating_sub(25);
        return Err(format!(
            "{err}\n--- Java latest.log tail ---\n{}",
            all_lines[start..].join("\n")
        ));
    }
    let _ = java_proc.send_console_command("stop").await;
    Ok(())
}

#[expect(
    clippy::too_many_lines,
    reason = "Coordinates external Vanilla Java structure placement, actions, and .nbt export"
)]
async fn record_contraptions_on_java_inner(
    java_proc: &mut JavaRecorderServer,
    temp_root: &Path,
    contraption_dirs: &[PathBuf],
) -> Result<(), String> {
    let parity_struct_dir = temp_root
        .join("world")
        .join("generated")
        .join("parity")
        .join("structure");
    fs::create_dir_all(&parity_struct_dir)
        .map_err(|e| format!("Failed to create {}: {e}", parity_struct_dir.display()))?;

    let ox = PARITY_ORIGIN.0.x;
    let oy = PARITY_ORIGIN.0.y;
    let oz = PARITY_ORIGIN.0.z;

    let mut bot = HeadlessBot::connect_and_join(java_proc.addr, "LockBot1").await?;
    let mut ping_nonce = 1000i64;
    let mut sequence_counter = 0i32;

    for cmd in [
        "op LockBot1".to_string(),
        "gamemode creative LockBot1".to_string(),
        "forceload add 144 144 192 192".to_string(),
        format!("tp LockBot1 {ox}.5 {oy}.0 {oz}.5 180 0"),
    ] {
        java_proc.send_console_command(&cmd).await?;
    }
    for _ in 0..4 {
        java_proc.wait_for_console_barrier().await?;
        drain_bot_until_pong(&mut bot, &mut ping_nonce, Duration::from_secs(5)).await?;
    }
    java_proc.send_console_command("tick freeze").await?;
    java_proc.wait_for_console_barrier().await?;
    java_proc.wait_for_console_barrier().await?;
    drain_bot_until_pong(&mut bot, &mut ping_nonce, Duration::from_secs(5)).await?;

    for dir in contraption_dirs {
        let name = dir
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown");
        let manifest_path = dir.join("test.json");
        let raw_manifest = fs::read_to_string(&manifest_path)
            .map_err(|e| format!("Failed to read {}: {e}", manifest_path.display()))?;
        let manifest: ContraptionTestManifest = serde_json::from_str(&raw_manifest)
            .map_err(|e| format!("Failed to parse {}: {e}", manifest_path.display()))?;

        let start_src = dir.join(&manifest.start);
        let start_bytes = fs::read(&start_src)
            .map_err(|e| format!("Failed to read {}: {e}", start_src.display()))?;
        let start_template = GameTestStructureTemplate::from_gzip_bytes(&start_bytes)
            .map_err(|e| format!("Failed to parse {}: {e}", start_src.display()))?;
        let size = start_template.size();

        let start_dst = parity_struct_dir.join(format!("{name}_start.nbt"));
        fs::copy(&start_src, &start_dst)
            .map_err(|e| format!("Failed to copy {}: {e}", start_src.display()))?;

        let bx1 = ox - 1;
        let by1 = oy - 1;
        let bz1 = oz - 1;
        let bx2 = ox + size[0];
        let by2 = oy + size[1];
        let bz2 = oz + size[2];
        let sb_pos = BlockPos::new(ox, oy - 1, oz - 1);

        for cmd in [
            format!("fill {bx1} {by1} {bz1} {bx2} {by2} {bz2} minecraft:air"),
            format!("place template parity:{name}_start {ox} {oy} {oz} none none 1.0 0 strict"),
            format!(
                "setblock {} {} {} minecraft:structure_block[mode=save]",
                sb_pos.0.x, sb_pos.0.y, sb_pos.0.z
            ),
        ] {
            java_proc.send_console_command(&cmd).await?;
        }
        java_proc.wait_for_console_barrier().await?;
        drain_bot_until_pong(&mut bot, &mut ping_nonce, Duration::from_secs(5)).await?;

        let max_tick = manifest
            .actions
            .iter()
            .map(|a| a.tick)
            .chain(manifest.checks.iter().map(|c| c.tick))
            .max()
            .unwrap_or(0);

        for tick in 0..=max_tick {
            if tick > 0 {
                java_proc.step_and_wait_tick().await?;
                drain_bot_until_pong(&mut bot, &mut ping_nonce, Duration::from_secs(5)).await?;
            }

            for action in manifest.actions.iter().filter(|a| a.tick == tick) {
                match &action.kind {
                    ContraptionActionKind::SetBlock { pos, block } => {
                        let wx = ox + pos[0];
                        let wy = oy + pos[1];
                        let wz = oz + pos[2];
                        java_proc
                            .send_console_command(&format!("setblock {wx} {wy} {wz} {block}"))
                            .await?;
                        java_proc.wait_for_console_barrier().await?;
                    }
                    ContraptionActionKind::BreakBlock { pos } => {
                        let wx = ox + pos[0];
                        let wy = oy + pos[1];
                        let wz = oz + pos[2];
                        java_proc
                            .send_console_command(&format!("setblock {wx} {wy} {wz} minecraft:air"))
                            .await?;
                        java_proc.wait_for_console_barrier().await?;
                    }
                    ContraptionActionKind::UseBlock { pos }
                    | ContraptionActionKind::PressButton { pos }
                    | ContraptionActionKind::PullLever { pos } => {
                        let wx = ox + pos[0];
                        let wy = oy + pos[1];
                        let wz = oz + pos[2];
                        let bot_pos =
                            Vector3::new(f64::from(wx) + 0.5, f64::from(wy), f64::from(wz) + 1.5);
                        java_proc
                            .send_console_command(&format!(
                                "tp LockBot1 {} {} {} 180 0",
                                bot_pos.x, bot_pos.y, bot_pos.z
                            ))
                            .await?;
                        java_proc.wait_for_console_barrier().await?;
                        java_proc.wait_for_console_barrier().await?;
                        drain_bot_until_pong(&mut bot, &mut ping_nonce, Duration::from_secs(5))
                            .await?;
                        java_proc.wait_for_console_barrier().await?;
                        java_proc.wait_for_console_barrier().await?;
                        sequence_counter += 1;
                        bot.send_packet(&SUseItemOn {
                            hand: VarInt(Hand::Left as i32),
                            position: BlockPos::new(wx, wy, wz),
                            face: VarInt(BlockDirection::Up.to_index() as i32),
                            cursor_pos: Vector3::new(0.5, 0.5, 0.5),
                            inside_block: false,
                            is_against_world_border: false,
                            sequence: VarInt(sequence_counter),
                        })
                        .await?;
                        drain_bot_until_pong(&mut bot, &mut ping_nonce, Duration::from_secs(5))
                            .await?;
                        java_proc.wait_for_console_barrier().await?;
                        java_proc.wait_for_console_barrier().await?;
                    }
                }
            }

            for check in manifest.checks.iter().filter(|c| c.tick == tick) {
                let save_name = format!("parity:{name}_t{tick}");
                bot.send_packet(&SSetStructureBlock {
                    location: sb_pos,
                    action: VarInt(SSetStructureBlock::ACTION_SAVE_AREA),
                    mode: VarInt(SSetStructureBlock::MODE_SAVE),
                    name: &save_name,
                    offset_x: 0,
                    offset_y: 1,
                    offset_z: 1,
                    size_x: u8::try_from(size[0]).unwrap_or(1),
                    size_y: u8::try_from(size[1]).unwrap_or(1),
                    size_z: u8::try_from(size[2]).unwrap_or(1),
                    mirror: VarInt(0),
                    rotation: VarInt(0),
                    metadata: "",
                    integrity: 1.0,
                    seed: VarLong(0),
                    flags: SSetStructureBlock::FLAG_IGNORE_ENTITIES,
                })
                .await?;
                drain_bot_until_pong(&mut bot, &mut ping_nonce, Duration::from_secs(5)).await?;
                java_proc.wait_for_console_barrier().await?;
                java_proc.wait_for_console_barrier().await?;

                let saved_nbt = parity_struct_dir.join(format!("{name}_t{tick}.nbt"));
                let dst_nbt = dir.join(&check.nbt);
                fs::copy(&saved_nbt, &dst_nbt).map_err(|e| {
                    format!(
                        "Failed to copy recorded structure {} to {}: {e}",
                        saved_nbt.display(),
                        dst_nbt.display()
                    )
                })?;
            }
        }
    }

    let _ = java_proc.send_console_command("stop").await;
    Ok(())
}

async fn drain_bot_until_pong(
    bot: &mut HeadlessBot,
    ping_nonce: &mut i64,
    wait_timeout: Duration,
) -> Result<(), String> {
    *ping_nonce += 1;
    let expected_nonce = *ping_nonce;
    bot.send_packet(&SPlayPingRequest {
        payload: expected_nonce,
    })
    .await?;

    let v = CURRENT_MC_VERSION;
    let pong_id = c_play::PONG_RESPONSE.to_id(v);
    let disconnect_id = c_play::DISCONNECT.to_id(v);
    let keep_alive_id = c_play::KEEP_ALIVE.to_id(v);
    let player_pos_id = c_play::PLAYER_POSITION.to_id(v);
    let chunk_batch_end_id = c_play::CHUNK_BATCH_FINISHED.to_id(v);

    loop {
        let pkt = match tokio::time::timeout(wait_timeout, bot.reader.get_raw_packet()).await {
            Ok(Ok(pkt)) => pkt,
            Ok(Err(e)) => return Err(format!("Bot '{}' packet decode error: {e}", bot.name)),
            Err(_) => {
                return Err(format!(
                    "Timed out after {wait_timeout:?} waiting for packet on {}",
                    bot.name
                ));
            }
        };
        let id = pkt.id;
        let data = &pkt.payload[..];
        if id == disconnect_id {
            return Err(format!(
                "Bot '{}' received DISCONNECT packet from server: {:?}",
                bot.name,
                String::from_utf8_lossy(data)
            ));
        }
        if id == pong_id {
            let mut slice = data;
            if let Ok(nonce) = slice.get_i64_be()
                && nonce == expected_nonce
            {
                break;
            }
            continue;
        }
        if id == keep_alive_id {
            let mut slice = data;
            if let Ok(ka) = CKeepAlive::read(&mut slice, &v) {
                bot.send_packet(&SKeepAlive {
                    keep_alive_id: ka.keep_alive_id,
                })
                .await?;
            }
            continue;
        }
        if id == chunk_batch_end_id {
            bot.send_packet(&SChunkBatch {
                chunks_per_tick: 25.0,
            })
            .await?;
            continue;
        }
        if id == player_pos_id {
            let mut slice = data;
            if let Ok(pos) = CPlayerPosition::read(&mut slice, &v) {
                if pos.relatives.contains(&PositionFlag::X) {
                    bot.position.x += pos.position.x;
                } else {
                    bot.position.x = pos.position.x;
                }
                if pos.relatives.contains(&PositionFlag::Y) {
                    bot.position.y += pos.position.y;
                } else {
                    bot.position.y = pos.position.y;
                }
                if pos.relatives.contains(&PositionFlag::Z) {
                    bot.position.z += pos.position.z;
                } else {
                    bot.position.z = pos.position.z;
                }
                if pos.relatives.contains(&PositionFlag::YRot) {
                    bot.yaw += pos.yaw;
                } else {
                    bot.yaw = pos.yaw;
                }
                if pos.relatives.contains(&PositionFlag::XRot) {
                    bot.pitch += pos.pitch;
                } else {
                    bot.pitch = pos.pitch;
                }
                bot.send_packet(&SConfirmTeleport {
                    teleport_id: pos.teleport_id,
                    position: bot.position,
                    yaw: bot.yaw,
                    pitch: bot.pitch,
                })
                .await?;
            }
        }
    }
    Ok(())
}

struct JavaRecorderServer {
    addr: SocketAddr,
    _child: Child,
    stdin: ChildStdin,
    stdout_rx: UnboundedReceiver<String>,
    temp_dir_path: PathBuf,
    _temp_dir: TempDir,
}

impl JavaRecorderServer {
    #[expect(
        clippy::too_many_lines,
        reason = "Configures and spawns external Vanilla Java server subprocess for recording"
    )]
    async fn spawn(jar_path: &Path) -> Result<Self, String> {
        if !jar_path.exists() {
            return Err(format!(
                "Java server jar does not exist at {}",
                jar_path.display()
            ));
        }

        let temp_dir = tempfile::tempdir()
            .map_err(|e| format!("Failed to create temp dir for Java server: {e}"))?;
        let temp_dir_path = temp_dir.path().to_path_buf();
        let listener = std::net::TcpListener::bind("127.0.0.1:0")
            .map_err(|e| format!("Failed to bind ephemeral port: {e}"))?;
        let port = listener
            .local_addr()
            .map_err(|e| format!("Failed to get local_addr: {e}"))?
            .port();
        drop(listener);
        let addr = SocketAddr::from(([127, 0, 0, 1], port));

        fs::write(temp_dir_path.join("eula.txt"), "eula=true\n")
            .map_err(|e| format!("Failed to write eula.txt: {e}"))?;

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
        fs::write(temp_dir_path.join("server.properties"), server_props)
            .map_err(|e| format!("Failed to write server.properties: {e}"))?;

        let java_bin = resolve_java_binary();
        let abs_jar = fs::canonicalize(jar_path)
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
            _child: child,
            stdin,
            stdout_rx,
            temp_dir_path,
            _temp_dir: temp_dir,
        };

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

    async fn send_console_command(&mut self, cmd: &str) -> Result<(), String> {
        self.stdin
            .write_all(format!("{cmd}\n").as_bytes())
            .await
            .map_err(|e| format!("Failed to send console command '{cmd}': {e}"))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| format!("Failed to flush console command '{cmd}': {e}"))
    }

    async fn wait_for_console_barrier(&mut self) -> Result<(), String> {
        self.send_console_command("seed").await?;
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let rem = deadline.saturating_duration_since(Instant::now());
            if rem.is_zero() {
                return Err("Timed out waiting for Java server console tick barrier".to_string());
            }
            match tokio::time::timeout(rem, self.stdout_rx.recv()).await {
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

    async fn step_and_wait_tick(&mut self) -> Result<(), String> {
        self.send_console_command("tick step 1").await?;
        self.wait_for_console_barrier().await?;
        self.wait_for_console_barrier().await?;
        self.wait_for_console_barrier().await?;
        Ok(())
    }
}

fn resolve_java_binary() -> PathBuf {
    resolve_option_path("--parity-java-bin", "PUMPKIN_PARITY_JAVA_BIN")
        .unwrap_or_else(|| PathBuf::from("java"))
}

fn resolve_option_path(cli_flag: &str, env_var: &str) -> Option<PathBuf> {
    let args: Vec<String> = std::env::args().collect();
    for window in args.windows(2) {
        if window[0] == cli_flag && !window[1].trim().is_empty() {
            return Some(PathBuf::from(&window[1]));
        }
    }
    if let Ok(val) = std::env::var(env_var)
        && !val.trim().is_empty()
    {
        return Some(PathBuf::from(val));
    }
    None
}

fn cli_has_flag(cli_flag: &str) -> bool {
    std::env::args().any(|arg| arg == cli_flag)
}

fn resolve_parity_dir() -> Option<PathBuf> {
    if let Some(path) = resolve_option_path("--parity-dir", "PUMPKIN_PARITY_DIR")
        && path.exists()
    {
        return Some(path);
    }

    let candidates = [
        PathBuf::from("assets/tests/redstone_parity"),
        PathBuf::from("../../assets/tests/redstone_parity"),
        PathBuf::from("code/Pumpkin/assets/tests/redstone_parity"),
    ];
    candidates.into_iter().find(|candidate| candidate.exists())
}

fn force_load_parity_chunks(world: &World, min_cx: i32, max_cx: i32, min_cz: i32, max_cz: i32) {
    let mut forced = world
        .forced_chunks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut chunk_loading = world
        .level
        .chunk_loading
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for cx in min_cx..=max_cx {
        for cz in min_cz..=max_cz {
            let chunk = Vector2::new(cx, cz);
            forced.insert(chunk);
            chunk_loading.add_force_ticket(chunk);
        }
    }
    chunk_loading.send_change();
}
