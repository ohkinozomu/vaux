mod codec;
mod connack;
mod connect;
pub mod disconnect;
mod fixed;
pub mod puback;
pub mod publish;
pub mod pubrec;
pub mod subscribe;
mod will;
mod property;

use crate::codec::{
    encode_utf8_string, encode_variable_len_integer, variable_byte_int_size, 
    PROP_SIZE_U32, PROP_SIZE_U8,
};

pub use crate::property::PropertyType;

pub use crate::codec::{
    decode, decode_fixed_header, encode, MQTTCodecError, Packet, PacketType, QoSLevel, Reason,
};
pub use crate::connack::ConnAck;
pub use crate::connect::Connect;
pub use crate::will::WillMessage;
pub use crate::{disconnect::Disconnect, fixed::FixedHeader, subscribe::Subscribe};
use bytes::{BufMut, BytesMut};
use std::collections::HashMap;

pub trait Size {
    fn size(&self) -> u32;
    fn property_size(&self) -> u32;
    fn payload_size(&self) -> u32;
}

pub trait Encode: Size {
    fn encode(&self, dest: &mut BytesMut) -> Result<(), MQTTCodecError>;
}

pub trait Decode {
    fn decode(&mut self, src: &mut BytesMut) -> Result<(), MQTTCodecError>;
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct UserPropertyMap {
    map: HashMap<String, Vec<String>>,
}

impl UserPropertyMap {
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
        }
    }

    pub fn map(&self) -> &HashMap<String, Vec<String>> {
        &self.map
    }

    pub fn add_property(&mut self, key: &str, value: &str) {
        if self.map.contains_key(key) {
            self.map.get_mut(key).unwrap().push(value.to_string());
        } else {
            let mut v: Vec<String> = Vec::new();
            v.push(value.to_string());
            self.map.insert(key.to_string(), v);
        }
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.map.contains_key(key)
    }
}

impl crate::Size for UserPropertyMap {
    fn size(&self) -> u32 {
        let mut remaining: u32 = 0;
        for (key, value) in self.map.iter() {
            let key_len = key.len() as u32 + 2;
            for v in value {
                remaining += key_len + v.len() as u32 + 3;
            }
        }
        remaining
    }

    fn property_size(&self) -> u32 {
        0
    }

    fn payload_size(&self) -> u32 {
        0
    }
}

impl Encode for UserPropertyMap {
    fn encode(&self, dest: &mut BytesMut) -> Result<(), MQTTCodecError> {
        for (k, value) in self.map.iter() {
            for v in value {
                dest.put_u8(PropertyType::UserProperty as u8);
                encode_utf8_string(k, dest)?;
                encode_utf8_string(&v, dest)?;
            }
        }
        Ok(())
    }
}
