use std::path::{Path, PathBuf};

use pumpkin_gametest::GameTestStructureTemplate;
use pumpkin_util::GameMode;
use serde::{Deserialize, Serialize};

use crate::lockstep::{
    client::{BotTarget, LockstepAction},
    peer::{InitialBlockPlacement, InitialInventoryItem, InitialSpawnedMob},
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LockstepStepSpec {
    pub step: usize,
    #[serde(default = "default_step_label")]
    pub label: String,
    #[serde(default)]
    pub actions: Vec<(BotTarget, LockstepAction)>,
}

fn default_step_label() -> String {
    "tick".to_string()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "check")]
#[expect(
    clippy::enum_variant_names,
    reason = "Expect* prefix clarifies declarative expectation rules"
)]
pub enum VanillaJavaExpectation {
    /// Expect at least one packet of `packet_kind` on `bot` at `step` (or within `[step, max_step]`).
    ExpectPacketKind {
        step: usize,
        max_step: usize,
        bot: BotTarget,
        packet_kind: String,
        description: String,
    },
    /// Expect `BlockChangedAck { sequence }` on Primary bot on the exact `step` when the block action occurred.
    ExpectSameTickBlockChangedAck {
        step: usize,
        sequence: i32,
        description: String,
    },
    /// Expect watched block at `offset` to start with `expected_state_prefix` at `step`.
    ExpectBlockStatePrefix {
        step: usize,
        offset: [i32; 3],
        expected_state_prefix: String,
        description: String,
    },
    /// Expect watched block at `offset` to contain `expected_substring` at `step`.
    ExpectBlockStateContains {
        step: usize,
        offset: [i32; 3],
        expected_substring: String,
        description: String,
    },
    /// Expect primary player gamemode at `step`.
    ExpectPlayerGamemode {
        step: usize,
        expected_gamemode: String,
        description: String,
    },
    /// Expect primary player sprinting/sneaking/flying flag at `step`.
    ExpectPlayerFlags {
        step: usize,
        sprinting: Option<bool>,
        sneaking: Option<bool>,
        flying: Option<bool>,
        description: String,
    },
    /// Expect primary player selected hotbar slot at `step`.
    ExpectSelectedSlot {
        step: usize,
        slot: u8,
        description: String,
    },
    /// Expect primary player inventory to contain `item` with at least `min_count` at `step`.
    ExpectInventoryHasItem {
        step: usize,
        item: String,
        min_count: u8,
        description: String,
    },
    /// Expect `SpawnEntity` with `entity_type` on `bot` within `[step, max_step]`.
    ExpectSpawnEntity {
        step: usize,
        max_step: usize,
        bot: BotTarget,
        entity_type: String,
        description: String,
    },
}

const fn default_observer_offset() -> [f64; 3] {
    [2.5, 0.0, 2.5]
}

const fn default_gamemode() -> GameMode {
    GameMode::Survival
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockstepScenario {
    pub id: String,
    #[serde(default = "default_category")]
    pub category: String,
    #[serde(default)]
    pub description: String,
    /// Optional total tick count to run. Any tick `t < duration_ticks` without an explicit entry
    /// in `steps` automatically runs an empty tick and compares both servers' outputs.
    #[serde(default)]
    pub duration_ticks: usize,
    /// Optional Minecraft `.nbt` structure template ID or filename in `assets/tests/lockstep_structures/`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structure: Option<String>,
    /// Offset relative to arena `origin` where `structure` is placed.
    #[serde(default)]
    pub structure_offset: [i32; 3],
    /// Optional path to a shared Minecraft Anvil world savegame directory loaded by both servers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub world_save: Option<PathBuf>,
    #[serde(default)]
    pub spawn_observer: bool,
    #[serde(default = "default_observer_offset")]
    pub observer_offset: [f64; 3],
    #[serde(default = "default_gamemode")]
    pub initial_gamemode: GameMode,
    #[serde(default)]
    pub include_sounds: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub initial_blocks: Vec<InitialBlockPlacement>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub initial_inventory: Vec<InitialInventoryItem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub initial_mobs: Vec<InitialSpawnedMob>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub watched_offsets: Vec<[i32; 3]>,
    #[serde(default)]
    pub steps: Vec<LockstepStepSpec>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub java_expectations: Vec<VanillaJavaExpectation>,
}

fn default_category() -> String {
    "custom".to_string()
}

impl LockstepScenario {
    /// Returns the total number of ticks this scenario executes (`max(duration_ticks, max_step + 1)`).
    #[must_use]
    pub fn total_ticks(&self) -> usize {
        let max_from_steps = self.steps.iter().map(|s| s.step + 1).max().unwrap_or(0);
        self.duration_ticks.max(max_from_steps)
    }

    /// Expands `steps` across `0..total_ticks()`, filling in passive ticks (`actions: []`) for any
    /// tick index not explicitly listed in `self.steps`.
    #[must_use]
    pub fn effective_steps(&self) -> Vec<LockstepStepSpec> {
        let total = self.total_ticks();
        let mut out = Vec::with_capacity(total);
        for tick in 0..total {
            if let Some(existing) = self.steps.iter().find(|s| s.step == tick) {
                out.push(existing.clone());
            } else {
                out.push(LockstepStepSpec {
                    step: tick,
                    label: format!("tick_{tick}"),
                    actions: Vec::new(),
                });
            }
        }
        out
    }
}

#[must_use]
pub fn default_lockstep_scenarios_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/tests/lockstep_scenarios")
}

#[must_use]
pub fn default_lockstep_structures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/tests/lockstep_structures")
}

/// Resolves a structure identifier (e.g. `"lockstep:interact_lever_and_stone_button_pulse"` or
/// `"my_structure.nbt"`) to its `.nbt` file path on disk.
pub fn resolve_structure_nbt_path(structure_id: &str) -> Result<PathBuf, String> {
    let direct = Path::new(structure_id);
    if direct.is_file() {
        return Ok(direct.to_path_buf());
    }
    let short_name = structure_id
        .split_once(':')
        .map_or(structure_id, |(_, path)| path);
    let file_name = if Path::new(short_name)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("nbt"))
    {
        short_name.to_string()
    } else {
        format!("{short_name}.nbt")
    };
    let candidate = default_lockstep_structures_dir().join(&file_name);
    if candidate.is_file() {
        return Ok(candidate);
    }
    Err(format!(
        "Lockstep structure '{structure_id}' not found at {}",
        candidate.display()
    ))
}

/// Loads a `.nbt` structure template for a lockstep scenario.
pub fn load_structure_template(structure_id: &str) -> Result<GameTestStructureTemplate, String> {
    let path = resolve_structure_nbt_path(structure_id)?;
    let bytes = std::fs::read(&path)
        .map_err(|e| format!("Failed to read structure {}: {e}", path.display()))?;
    GameTestStructureTemplate::from_gzip_bytes(&bytes)
        .map_err(|e| format!("Failed to parse structure {}: {e}", path.display()))
}

/// Loads lockstep scenarios from a JSON file or a directory of `*.json` files.
pub fn load_scenarios_from_path(path: &Path) -> Result<Vec<LockstepScenario>, String> {
    if path.is_file() {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read scenario file {}: {e}", path.display()))?;
        if raw.trim_start().starts_with('[') {
            return serde_json::from_str::<Vec<LockstepScenario>>(&raw)
                .map_err(|e| format!("Failed to parse scenario list {}: {e}", path.display()));
        }
        let single = serde_json::from_str::<LockstepScenario>(&raw)
            .map_err(|e| format!("Failed to parse scenario {}: {e}", path.display()))?;
        return Ok(vec![single]);
    }

    if !path.is_dir() {
        return Err(format!(
            "Lockstep scenarios path does not exist: {}",
            path.display()
        ));
    }

    let mut json_files: Vec<PathBuf> = std::fs::read_dir(path)
        .map_err(|e| format!("Failed to read directory {}: {e}", path.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|p| {
            p.is_file()
                && p.extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
        })
        .collect();
    json_files.sort();

    let mut scenarios = Vec::new();
    for file in json_files {
        let mut loaded = load_scenarios_from_path(&file)?;
        scenarios.append(&mut loaded);
    }
    Ok(scenarios)
}

/// Builds a passive multi-tick scenario that loads a shared Minecraft Anvil world savegame
/// and ticks both servers in lockstep for `duration_ticks`.
#[must_use]
pub fn scenario_from_world_save(world_save: PathBuf, duration_ticks: usize) -> LockstepScenario {
    let folder_name = world_save
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("world_save");
    let ticks = duration_ticks.max(1);
    LockstepScenario {
        id: format!("lockstep:world_save_{folder_name}"),
        category: "world_save".to_string(),
        description: format!(
            "Shared Minecraft world savegame '{}' ticked in lockstep for {ticks} ticks",
            world_save.display()
        ),
        duration_ticks: ticks,
        structure: None,
        structure_offset: [0, 0, 0],
        world_save: Some(world_save),
        spawn_observer: true,
        observer_offset: [2.5, 0.0, 2.5],
        initial_gamemode: GameMode::Survival,
        include_sounds: false,
        initial_blocks: Vec::new(),
        initial_inventory: Vec::new(),
        initial_mobs: Vec::new(),
        watched_offsets: vec![[0, -1, 0], [0, 0, 1], [0, 0, 2]],
        steps: Vec::new(),
        java_expectations: Vec::new(),
    }
}

#[must_use]
pub fn all_lockstep_scenarios() -> Vec<LockstepScenario> {
    let dir = default_lockstep_scenarios_dir();
    load_scenarios_from_path(&dir).unwrap_or_default()
}
