use std::collections::BTreeMap;
use std::fmt;

use pumpkin_data::{Block, BlockState, BlockStateId, Property, PropertyKey};
use pumpkin_nbt::{
    NbtCompound,
    nbt_compress::{read_gzip_compound_tag, write_gzip_compound_tag_to_bytes},
    tag::NbtTag,
};
use serde_json::{Map, Value};

use crate::error::{GameTestError, GameTestResult};

/// Formats a `BlockStateId` as a canonical Vanilla Java block state string:
/// `minecraft:<name>` or `minecraft:<name>[k1=v1,k2=v2]` with properties sorted by key.
#[must_use]
pub fn format_canonical_block_state(state_id: BlockStateId) -> String {
    let (block, _) = BlockState::from_id_with_block(state_id);
    let base_name = format!("minecraft:{}", block.name);
    let Some(properties) = block.properties(state_id) else {
        return base_name;
    };

    let props = properties.to_props();
    if props.is_empty() {
        return base_name;
    }

    let mut pairs: Vec<(&str, &str)> = props
        .iter()
        .map(|prop| (prop.key_str(), prop.value_str()))
        .collect();
    pairs.sort_unstable_by(|left, right| left.0.cmp(right.0));

    let mut out = String::with_capacity(base_name.len() + pairs.len() * 12 + 2);
    out.push_str(&base_name);
    out.push('[');
    for (idx, (key, value)) in pairs.iter().enumerate() {
        if idx > 0 {
            out.push(',');
        }
        out.push_str(key);
        out.push('=');
        out.push_str(value);
    }
    out.push(']');
    out
}

/// Parses a canonical Vanilla Java block state string (`minecraft:<name>[k1=v1,...]`)
/// back into a `BlockStateId`.
///
/// # Errors
///
/// Returns `GameTestError::InvalidStructure` if the block name or property specification is invalid.
pub fn parse_canonical_block_state(canonical: &str) -> GameTestResult<BlockStateId> {
    let (name_part, props_part) = if let Some((name, rest)) = canonical.split_once('[') {
        let props = rest.strip_suffix(']').ok_or_else(|| {
            GameTestError::InvalidStructure(format!(
                "Canonical block state '{canonical}' is missing closing ']'"
            ))
        })?;
        (name, Some(props))
    } else {
        (canonical, None)
    };

    let block = Block::from_name(name_part).ok_or_else(|| {
        GameTestError::InvalidStructure(format!(
            "Unknown block '{name_part}' in canonical block state '{canonical}'"
        ))
    })?;

    let Some(props_str) = props_part else {
        return Ok(block.default_state.id);
    };
    if props_str.is_empty() {
        return Ok(block.default_state.id);
    }

    let mut prop_map: BTreeMap<&str, Property> = BTreeMap::new();
    if let Some(default_props) = block.properties(block.default_state.id) {
        for prop in default_props.to_props() {
            prop_map.insert(prop.key_str(), prop);
        }
    }

    for entry in props_str.split(',') {
        let (key_name, value_name) = entry.split_once('=').ok_or_else(|| {
            GameTestError::InvalidStructure(format!(
                "Invalid property '{entry}' in canonical block state '{canonical}'"
            ))
        })?;
        let key = PropertyKey::from_str(key_name).ok_or_else(|| {
            GameTestError::InvalidStructure(format!(
                "Unknown property key '{key_name}' for '{name_part}'"
            ))
        })?;
        let prop = Property::from_key_value_str(key, value_name).ok_or_else(|| {
            GameTestError::InvalidStructure(format!(
                "Invalid property value '{value_name}' for key '{key_name}' on '{name_part}'"
            ))
        })?;
        prop_map.insert(prop.key_str(), prop);
    }

    let parsed_props: Vec<Property> = prop_map.into_values().collect();
    let state = block.state_from_properties(&parsed_props).ok_or_else(|| {
        GameTestError::InvalidStructure(format!(
            "No block state matches canonical specification '{canonical}'"
        ))
    })?;
    Ok(state.id)
}

/// Converts a block-entity `NbtCompound` into a canonical JSON value with sorted keys
/// and structure-relative `x`, `y`, `z` coordinates.
#[must_use]
pub fn nbt_compound_to_relative_json(nbt: &NbtCompound, relative_pos: [i32; 3]) -> Value {
    let mut normalized = nbt.clone();
    normalized.put_int("x", relative_pos[0]);
    normalized.put_int("y", relative_pos[1]);
    normalized.put_int("z", relative_pos[2]);
    nbt_compound_to_json_inner(&normalized)
}

fn nbt_compound_to_json_inner(nbt: &NbtCompound) -> Value {
    let mut sorted: BTreeMap<&str, &NbtTag> = BTreeMap::new();
    for (key, value) in &nbt.child_tags {
        if key.as_ref() == "LastOutput" {
            continue;
        }
        if key.as_ref() == "components"
            && matches!(value, NbtTag::Compound(c) if c.child_tags.is_empty())
        {
            continue;
        }
        sorted.insert(key.as_ref(), value);
    }

    let mut map = Map::with_capacity(sorted.len());
    for (key, value) in sorted {
        map.insert(key.to_owned(), nbt_tag_to_json(value));
    }
    Value::Object(map)
}

fn nbt_tag_to_json(tag: &NbtTag) -> Value {
    match tag {
        NbtTag::End => Value::Null,
        NbtTag::Byte(v) => Value::from(*v),
        NbtTag::Short(v) => Value::from(*v),
        NbtTag::Int(v) => Value::from(*v),
        NbtTag::Long(v) => Value::from(*v),
        NbtTag::Float(v) => Value::from(round_f64(f64::from(*v))),
        NbtTag::Double(v) => Value::from(round_f64(*v)),
        NbtTag::ByteArray(bytes) => Value::Array(bytes.iter().copied().map(Value::from).collect()),
        NbtTag::String(s) => Value::String(s.to_string()),
        NbtTag::List(items) => Value::Array(items.iter().map(nbt_tag_to_json).collect()),
        NbtTag::Compound(compound) => nbt_compound_to_json_inner(compound),
        NbtTag::IntArray(ints) => Value::Array(ints.iter().copied().map(Value::from).collect()),
        NbtTag::LongArray(longs) => Value::Array(longs.iter().copied().map(Value::from).collect()),
    }
}

fn round_f64(value: f64) -> f64 {
    if !value.is_finite() {
        return 0.0;
    }
    let rounded = (value * 10_000.0).round() / 10_000.0;
    if rounded == -0.0 { 0.0 } else { rounded }
}

/// Mismatch found when comparing an actual structure volume against an expected `.nbt` template.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StructureMismatch {
    Size {
        expected: [i32; 3],
        actual: [i32; 3],
    },
    BlockState {
        pos: [i32; 3],
        expected: String,
        actual: String,
    },
    BlockEntity {
        pos: [i32; 3],
        expected: String,
        actual: String,
    },
}

impl fmt::Display for StructureMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Size { expected, actual } => {
                write!(f, "size mismatch: expected {expected:?}, got {actual:?}")
            }
            Self::BlockState {
                pos,
                expected,
                actual,
            } => {
                write!(f, "block {pos:?}: expected '{expected}', got '{actual}'")
            }
            Self::BlockEntity {
                pos,
                expected,
                actual,
            } => {
                write!(f, "block_entity {pos:?}: expected {expected}, got {actual}")
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TestBlockMode {
    Start,
    Log,
    Fail,
    Accept,
}

impl TestBlockMode {
    #[must_use]
    pub const fn serialized_name(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Log => "log",
            Self::Fail => "fail",
            Self::Accept => "accept",
        }
    }

    #[must_use]
    pub const fn from_serialized_name(name: &str) -> Option<Self> {
        match name.as_bytes() {
            b"start" => Some(Self::Start),
            b"log" => Some(Self::Log),
            b"fail" => Some(Self::Fail),
            b"accept" => Some(Self::Accept),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct GameTestStructureBlock {
    pub position: [i32; 3],
    pub state: BlockStateId,
    pub nbt: Option<NbtCompound>,
    pub test_mode: Option<TestBlockMode>,
}

#[derive(Clone, Debug)]
pub struct GameTestStructureTemplate {
    size: [i32; 3],
    blocks: Vec<GameTestStructureBlock>,
}

#[derive(Clone, Copy, Debug)]
struct PaletteEntry {
    state: BlockStateId,
    test_mode: Option<TestBlockMode>,
}

impl GameTestStructureTemplate {
    pub fn new(size: [i32; 3], blocks: Vec<GameTestStructureBlock>) -> GameTestResult<Self> {
        if size.iter().any(|axis| *axis <= 0) {
            return Err(invalid_structure(format!(
                "Structure has invalid size {size:?}"
            )));
        }

        for (index, block) in blocks.iter().enumerate() {
            let position = block.position;
            if position[0] < 0
                || position[1] < 0
                || position[2] < 0
                || position[0] >= size[0]
                || position[1] >= size[1]
                || position[2] >= size[2]
            {
                return Err(invalid_structure(format!(
                    "Structure block {index} position {position:?} is outside size {size:?}"
                )));
            }
        }

        Ok(Self { size, blocks })
    }

    pub fn empty(size: [i32; 3]) -> GameTestResult<Self> {
        Self::new(size, Vec::new())
    }

    /// Parses synthetic empty structure identifiers of the form `[namespace:]empty_<W>x<H>x<D>`
    /// or `[namespace:]empty` (`1x1x1`).
    #[must_use]
    pub fn parse_empty_id(resource_location: &str) -> Option<Self> {
        let path = resource_location
            .split_once(':')
            .map_or(resource_location, |(_, path)| path);
        if path == "empty" {
            return Self::empty([1, 1, 1]).ok();
        }
        let dims = path.strip_prefix("empty_")?;
        let mut parts = dims.split('x');
        let width = parts.next()?.parse::<i32>().ok()?;
        let height = parts.next()?.parse::<i32>().ok()?;
        let depth = parts.next()?.parse::<i32>().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Self::empty([width, height, depth]).ok()
    }

    pub fn from_nbt(structure: &NbtCompound) -> GameTestResult<Self> {
        let size = read_vec3(structure, "size")?;
        if size.iter().any(|axis| *axis <= 0) {
            return Err(invalid_structure(format!(
                "Structure has invalid size {size:?}"
            )));
        }

        let palette = resolve_palette(structure)?;
        let blocks = structure
            .get_list("blocks")
            .ok_or_else(|| invalid_structure("Structure is missing 'blocks'"))?;
        let mut parsed_blocks = Vec::with_capacity(blocks.len());

        // Validate the complete structure before changing the world so malformed NBT
        // cannot leave a half-placed test behind.
        for (index, block) in blocks.iter().enumerate() {
            let block = block.extract_compound().ok_or_else(|| {
                invalid_structure(format!("Structure block {index} is not a compound"))
            })?;
            let position = read_vec3(block, "pos")?;
            if position[0] < 0
                || position[1] < 0
                || position[2] < 0
                || position[0] >= size[0]
                || position[1] >= size[1]
                || position[2] >= size[2]
            {
                return Err(invalid_structure(format!(
                    "Structure block {index} position {position:?} is outside size {size:?}"
                )));
            }

            let state_index = block.get_int("state").ok_or_else(|| {
                invalid_structure(format!(
                    "Structure block {index} is missing integer 'state'"
                ))
            })?;
            let state_index = usize::try_from(state_index).map_err(|_| {
                invalid_structure(format!("Structure block {index} has negative state index"))
            })?;
            let palette_entry = palette.get(state_index).copied().ok_or_else(|| {
                invalid_structure(format!(
                    "Structure block {index} references missing palette state {state_index}"
                ))
            })?;

            parsed_blocks.push(GameTestStructureBlock {
                position,
                state: palette_entry.state,
                nbt: block.get_compound("nbt").cloned(),
                test_mode: palette_entry.test_mode,
            });
        }

        Ok(Self {
            size,
            blocks: parsed_blocks,
        })
    }

    #[must_use]
    pub const fn size(&self) -> [i32; 3] {
        self.size
    }

    #[must_use]
    pub fn blocks(&self) -> &[GameTestStructureBlock] {
        &self.blocks
    }

    #[must_use]
    pub const fn block_count(&self) -> usize {
        self.blocks.len()
    }

    /// Serializes this structure template into a Vanilla-compatible structure `NbtCompound`
    /// with both modern (`id`/`properties`) and legacy (`Name`/`Properties`) palette fields.
    #[must_use]
    pub fn to_nbt(&self) -> NbtCompound {
        let mut palette_states: Vec<(BlockStateId, Option<TestBlockMode>)> = Vec::new();
        let mut palette_tags = Vec::new();
        let mut block_tags = Vec::with_capacity(self.blocks.len());

        for block in &self.blocks {
            let key = (block.state, block.test_mode);
            let state_idx = palette_states
                .iter()
                .position(|existing| *existing == key)
                .unwrap_or_else(|| {
                    let idx = palette_states.len();
                    palette_states.push(key);
                    palette_tags.push(NbtTag::Compound(build_palette_entry_nbt(
                        block.state,
                        block.test_mode,
                    )));
                    idx
                });

            let mut block_nbt = NbtCompound::new();
            block_nbt.put(
                "pos",
                NbtTag::List(vec![
                    NbtTag::Int(block.position[0]),
                    NbtTag::Int(block.position[1]),
                    NbtTag::Int(block.position[2]),
                ]),
            );
            block_nbt.put_int("state", i32::try_from(state_idx).unwrap_or(0));
            if let Some(nbt) = &block.nbt {
                let mut stripped = nbt.clone();
                stripped
                    .child_tags
                    .retain(|k, _| !matches!(k.as_ref(), "x" | "y" | "z"));
                block_nbt.put_compound("nbt", stripped);
            }
            block_tags.push(NbtTag::Compound(block_nbt));
        }

        if palette_tags.is_empty() {
            palette_tags.push(NbtTag::Compound(build_palette_entry_nbt(
                Block::AIR.default_state.id,
                None,
            )));
        }

        let mut root = NbtCompound::new();
        root.put(
            "size",
            NbtTag::List(vec![
                NbtTag::Int(self.size[0]),
                NbtTag::Int(self.size[1]),
                NbtTag::Int(self.size[2]),
            ]),
        );
        root.put("palette", NbtTag::List(palette_tags));
        root.put("blocks", NbtTag::List(block_tags));
        root.put("entities", NbtTag::List(Vec::new()));
        root.put_int("DataVersion", 4325);
        root
    }

    /// Compares this (actual) structure template against an `expected` `.nbt` template.
    ///
    /// Checks block states across the entire `0..size` volume (treating omitted positions as
    /// `minecraft:air` and skipping `minecraft:structure_void` in `expected`), and compares
    /// block-entity NBT whenever the `expected` template includes it at `[x, y, z]`.
    #[must_use]
    pub fn compare(&self, expected: &Self) -> Vec<StructureMismatch> {
        if self.size != expected.size {
            return vec![StructureMismatch::Size {
                expected: expected.size,
                actual: self.size,
            }];
        }

        let mut expected_map: BTreeMap<[i32; 3], &GameTestStructureBlock> = BTreeMap::new();
        for block in &expected.blocks {
            expected_map.insert(block.position, block);
        }

        let mut actual_map: BTreeMap<[i32; 3], &GameTestStructureBlock> = BTreeMap::new();
        for block in &self.blocks {
            actual_map.insert(block.position, block);
        }

        let air_id = Block::AIR.default_state.id;
        let structure_void_id = Block::STRUCTURE_VOID.default_state.id;
        let mut mismatches = Vec::new();

        for ry in 0..self.size[1] {
            for rz in 0..self.size[2] {
                for rx in 0..self.size[0] {
                    let pos = [rx, ry, rz];
                    let exp_block = expected_map.get(&pos).copied();
                    let exp_state = exp_block.map_or(air_id, |b| b.state);
                    if exp_state == structure_void_id {
                        continue;
                    }

                    let act_block = actual_map.get(&pos).copied();
                    let act_state = act_block.map_or(air_id, |b| b.state);

                    if exp_state != act_state {
                        mismatches.push(StructureMismatch::BlockState {
                            pos,
                            expected: format_canonical_block_state(exp_state),
                            actual: format_canonical_block_state(act_state),
                        });
                    }

                    if let Some(exp_nbt) = exp_block.and_then(|b| b.nbt.as_ref()) {
                        let exp_json = nbt_compound_to_relative_json(exp_nbt, pos);
                        match act_block.and_then(|b| b.nbt.as_ref()) {
                            Some(act_nbt) => {
                                let act_json = nbt_compound_to_relative_json(act_nbt, pos);
                                if !json_expected_matches(&exp_json, &act_json) {
                                    mismatches.push(StructureMismatch::BlockEntity {
                                        pos,
                                        expected: exp_json.to_string(),
                                        actual: act_json.to_string(),
                                    });
                                }
                            }
                            None => {
                                mismatches.push(StructureMismatch::BlockEntity {
                                    pos,
                                    expected: exp_json.to_string(),
                                    actual: "<none>".to_string(),
                                });
                            }
                        }
                    }
                }
            }
        }

        mismatches
    }

    /// Serializes this structure template as gzip-compressed `.nbt` bytes suitable for
    /// Vanilla Java and Pumpkin datapack `data/<namespace>/structure/<name>.nbt` files.
    ///
    /// # Errors
    ///
    /// Returns `GameTestError::InvalidStructure` if NBT serialization fails.
    pub fn to_gzip_bytes(&self) -> GameTestResult<Vec<u8>> {
        write_gzip_compound_tag_to_bytes(self.to_nbt())
            .map_err(|err| GameTestError::InvalidStructure(err.to_string()))
    }

    /// Deserializes a gzip-compressed `.nbt` structure file into a `GameTestStructureTemplate`.
    pub fn from_gzip_bytes(bytes: &[u8]) -> GameTestResult<Self> {
        let compound = read_gzip_compound_tag(std::io::Cursor::new(bytes))
            .map_err(|err| GameTestError::InvalidStructure(err.to_string()))?;
        Self::from_nbt(&compound)
    }
}

fn json_expected_matches(expected: &Value, actual: &Value) -> bool {
    match (expected, actual) {
        (Value::Object(exp_map), Value::Object(act_map)) => exp_map.iter().all(|(k, exp_val)| {
            act_map
                .get(k)
                .is_some_and(|act_val| json_expected_matches(exp_val, act_val))
        }),
        (Value::Array(exp_arr), Value::Array(act_arr)) => {
            exp_arr.len() == act_arr.len()
                && exp_arr
                    .iter()
                    .zip(act_arr.iter())
                    .all(|(e, a)| json_expected_matches(e, a))
        }
        _ => expected == actual,
    }
}

fn build_palette_entry_nbt(
    state_id: BlockStateId,
    test_mode: Option<TestBlockMode>,
) -> NbtCompound {
    let (block, _) = BlockState::from_id_with_block(state_id);
    let block_id = format!("minecraft:{}", block.name);
    let mut entry = NbtCompound::new();
    entry.put_string("id", block_id.clone());
    entry.put_string("Name", block_id);

    let mut props_compound = NbtCompound::new();
    if let Some(props) = block.properties(state_id) {
        let mut pairs: Vec<(&str, &str)> = props
            .to_props()
            .iter()
            .map(|prop| (prop.key_str(), prop.value_str()))
            .collect();
        pairs.sort_unstable_by(|a, b| a.0.cmp(b.0));
        for (k, v) in pairs {
            props_compound.put_string(k, v.to_owned());
        }
    }
    if let Some(mode) = test_mode {
        props_compound.put_string("mode", mode.serialized_name().to_owned());
    }
    if !props_compound.child_tags.is_empty() {
        entry.put_compound("properties", props_compound.clone());
        entry.put_compound("Properties", props_compound);
    }
    entry
}

fn read_vec3(compound: &NbtCompound, name: &str) -> GameTestResult<[i32; 3]> {
    let values = compound
        .get_list(name)
        .ok_or_else(|| invalid_structure(format!("Structure is missing '{name}' int list")))?;
    let [x, y, z] = values else {
        return Err(invalid_structure(format!(
            "Structure '{name}' must contain exactly three integers"
        )));
    };

    Ok([
        x.extract_int().ok_or_else(|| {
            invalid_structure(format!("Structure '{name}' contains a non-integer value"))
        })?,
        y.extract_int().ok_or_else(|| {
            invalid_structure(format!("Structure '{name}' contains a non-integer value"))
        })?,
        z.extract_int().ok_or_else(|| {
            invalid_structure(format!("Structure '{name}' contains a non-integer value"))
        })?,
    ])
}

fn resolve_palette(structure: &NbtCompound) -> GameTestResult<Vec<PaletteEntry>> {
    let palette = structure
        .get_list("palette")
        .ok_or_else(|| invalid_structure("Structure is missing 'palette'"))?;
    let mut states = Vec::with_capacity(palette.len());

    for (index, entry) in palette.iter().enumerate() {
        let entry = entry
            .extract_compound()
            .ok_or_else(|| invalid_structure(format!("Palette entry {index} is not a compound")))?;
        // 26.3 renamed palette keys from "Name"/"Properties" to "id"/"properties".
        let name = entry
            .get_string("id")
            .or_else(|| entry.get_string("Name"))
            .ok_or_else(|| invalid_structure(format!("Palette entry {index} is missing 'Name'")))?;
        let block = Block::from_name(name).ok_or_else(|| {
            invalid_structure(format!("Unknown block '{name}' in structure palette"))
        })?;

        let mut test_mode = None;
        let state = if let Some(properties) = entry
            .get_compound("properties")
            .or_else(|| entry.get_compound("Properties"))
        {
            let mut parsed_properties = Vec::with_capacity(properties.child_tags.len());
            for (property_name, property_value) in &properties.child_tags {
                let property_value = property_value.extract_string().ok_or_else(|| {
                    invalid_structure(format!(
                        "Block '{name}' property '{property_name}' in palette entry {index} is not a string"
                    ))
                })?;
                let property_key =
                    pumpkin_data::PropertyKey::from_str(property_name.as_ref()).ok_or_else(|| {
                        invalid_structure(format!(
                            "Unknown block property '{property_name}' for '{name}' in palette entry {index}"
                        ))
                    })?;
                if block == &Block::TEST_BLOCK && property_key == pumpkin_data::PropertyKey::Mode {
                    test_mode = Some(TestBlockMode::from_serialized_name(property_value).ok_or_else(
                        || {
                            invalid_structure(format!(
                                "Unknown test block mode '{property_value}' in palette entry {index}"
                            ))
                        },
                    )?);
                }
                let prop = pumpkin_data::Property::from_key_value_str(property_key, property_value)
                    .ok_or_else(|| {
                        invalid_structure(format!(
                            "No Pumpkin block state matches palette entry {index} for '{name}'"
                        ))
                    })?;
                parsed_properties.push(prop);
            }

            block
                .state_from_properties(&parsed_properties)
                .ok_or_else(|| {
                    invalid_structure(format!(
                        "No Pumpkin block state matches palette entry {index} for '{name}'"
                    ))
                })?
        } else {
            block.default_state
        };

        if block == &Block::TEST_BLOCK && test_mode.is_none() {
            // TestBlockMode.START is the first/default enum value in the 26.2 server source.
            test_mode = Some(TestBlockMode::Start);
        }

        states.push(PaletteEntry {
            state: state.id,
            test_mode,
        });
    }

    Ok(states)
}

fn invalid_structure(message: impl Into<String>) -> GameTestError {
    GameTestError::InvalidStructure(message.into())
}
