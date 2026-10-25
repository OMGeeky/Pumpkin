use pumpkin_data::packet::clientbound::play::MOVE_ENTITY_POS;
use pumpkin_macros::java_packet;
use pumpkin_util::math::vector3::Vector3;

use crate::{
    ClientPacket, ServerPacket, VarInt,
    ser::{NetworkReadExt, NetworkWriteExt, ReadingError, WritingError},
};
use pumpkin_util::version::JavaMinecraftVersion;

#[java_packet(MOVE_ENTITY_POS)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CUpdateEntityPos {
    pub entity_id: VarInt,
    pub delta: Vector3<i16>,
    pub on_ground: bool,
}

impl CUpdateEntityPos {
    #[must_use]
    pub const fn new(entity_id: VarInt, delta: Vector3<i16>, on_ground: bool) -> Self {
        Self {
            entity_id,
            delta,
            on_ground,
        }
    }
}

impl ClientPacket for CUpdateEntityPos {
    fn write_packet_data(
        &self,
        mut write: impl std::io::Write,
        version: &JavaMinecraftVersion,
    ) -> Result<(), WritingError> {
        if *version <= JavaMinecraftVersion::V_1_7_6 {
            write.write_i32_be(self.entity_id.0)?;
        } else {
            write.write_var_int(&self.entity_id)?;
        }
        // Since 26.3 the on ground flag and the delta step count are packed into a var int in
        // front of the delta, 0 steps being a single linear delta.
        if *version >= JavaMinecraftVersion::V_26_3 {
            write.write_var_int(&VarInt(i32::from(self.on_ground)))?;
        }
        if *version >= JavaMinecraftVersion::V_1_9 {
            write.write_i16_be(self.delta.x)?;
            write.write_i16_be(self.delta.y)?;
            write.write_i16_be(self.delta.z)?;
        } else {
            write.write_i8((self.delta.x / 128) as i8)?;
            write.write_i8((self.delta.y / 128) as i8)?;
            write.write_i8((self.delta.z / 128) as i8)?;
        }
        if *version >= JavaMinecraftVersion::V_1_8 && *version < JavaMinecraftVersion::V_26_3 {
            write.write_bool(self.on_ground)?;
        }
        Ok(())
    }
}

impl<'a> ServerPacket<'a> for CUpdateEntityPos {
    fn read(bytebuf: &mut &'a [u8], version: &JavaMinecraftVersion) -> Result<Self, ReadingError> {
        let entity_id = if *version <= JavaMinecraftVersion::V_1_7_6 {
            VarInt(bytebuf.get_i32_be()?)
        } else {
            bytebuf.get_var_int()?
        };
        let (on_ground_prefix, step_count) = if *version >= JavaMinecraftVersion::V_26_3 {
            let props = bytebuf.get_var_int()?.0;
            (Some(props & 1 != 0), (props as u32) >> 1)
        } else {
            (None, 0)
        };
        let delta = if step_count > 0 {
            let mut dx = 0i16;
            let mut dy = 0i16;
            let mut dz = 0i16;
            for _ in 0..step_count {
                let _ticks = bytebuf.get_var_int()?;
                dx = dx.saturating_add(bytebuf.get_i16_be()?);
                dy = dy.saturating_add(bytebuf.get_i16_be()?);
                dz = dz.saturating_add(bytebuf.get_i16_be()?);
            }
            Vector3::new(dx, dy, dz)
        } else if *version >= JavaMinecraftVersion::V_1_9 {
            Vector3::new(
                bytebuf.get_i16_be()?,
                bytebuf.get_i16_be()?,
                bytebuf.get_i16_be()?,
            )
        } else {
            Vector3::new(
                i16::from(bytebuf.get_i8()?) * 128,
                i16::from(bytebuf.get_i8()?) * 128,
                i16::from(bytebuf.get_i8()?) * 128,
            )
        };
        let on_ground = if let Some(on_ground) = on_ground_prefix {
            on_ground
        } else if *version >= JavaMinecraftVersion::V_1_8 {
            bytebuf.get_bool()?
        } else {
            false
        };
        Ok(Self {
            entity_id,
            delta,
            on_ground,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_v26_3_and_v1_21_11() {
        let pkt = CUpdateEntityPos::new(VarInt(42), Vector3::new(128, -64, 256), true);
        for version in [
            JavaMinecraftVersion::V_1_21_11,
            JavaMinecraftVersion::V_26_3,
        ] {
            let mut buf = Vec::new();
            pkt.write_packet_data(&mut buf, &version).unwrap();
            let mut slice = buf.as_slice();
            let decoded = CUpdateEntityPos::read(&mut slice, &version).unwrap();
            assert_eq!(decoded, pkt);
            assert!(slice.is_empty());
        }
    }
}
