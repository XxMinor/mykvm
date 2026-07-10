use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const LEFT_BUTTON_MASK: u64 = 1;
pub const RIGHT_BUTTON_MASK: u64 = 1 << 1;
pub const MIDDLE_BUTTON_MASK: u64 = 1 << 2;
pub const SHIFT_MODIFIER_MASK: u8 = 1;
pub const CONTROL_MODIFIER_MASK: u8 = 1 << 1;
pub const ALT_MODIFIER_MASK: u8 = 1 << 2;
pub const META_MODIFIER_MASK: u8 = 1 << 3;
pub const INPUT_PIPE_PREFIX: &str = r"\\.\pipe\mykvm-input-s";
pub const INPUT_SERVICE_NAME: &str = "MyKVMInputService";
pub const INPUT_SERVICE_DISPLAY_NAME: &str = "MyKVM Lock Screen Input Service";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum InputEvent {
    MouseMove {
        screen_id: String,
        x: i32,
        y: i32,
        /// Present while a button is held. Drag positions are latest-wins;
        /// `button_mask` below lets the receiver reconcile cross-channel order.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        drag_button: Option<MouseButton>,
        /// Authoritative held-button state from new senders. This lets the
        /// receiver repair a lost button up/down on the next mouse move.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        button_mask: Option<u64>,
        #[serde(default, skip_serializing_if = "is_zero")]
        sequence: u64,
    },
    MouseButton {
        button: MouseButton,
        down: bool,
        /// Button coordinates travel with the reliable button event. Older
        /// peers omit these fields and fall back to their last mouse move.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        screen_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        x: Option<i32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        y: Option<i32>,
        #[serde(default, skip_serializing_if = "is_zero")]
        sequence: u64,
    },
    Scroll {
        delta_x: i32,
        delta_y: i32,
        #[serde(default, skip_serializing_if = "is_zero")]
        sequence: u64,
    },
    Key {
        key_code: u16,
        down: bool,
    },
    /// Control has left this client: tuck the controlled cursor away at (x, y)
    /// and, where supported, hide it — it reappears on the next injected move or
    /// as soon as the local user physically moves the mouse.
    CursorPark {
        screen_id: String,
        x: i32,
        y: i32,
        #[serde(default, skip_serializing_if = "is_zero")]
        sequence: u64,
    },
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum InputCommand {
    MouseMove {
        x: i32,
        y: i32,
        drag_button: Option<MouseButton>,
    },
    MouseButton {
        button: MouseButton,
        down: bool,
        x: i32,
        y: i32,
    },
    Scroll {
        delta_x: i32,
        delta_y: i32,
    },
    Key {
        key_code: u16,
        down: bool,
    },
    CursorPark {
        x: i32,
        y: i32,
    },
    ReleaseAll,
    SecureAttention,
}

pub fn input_pipe_name(session_id: u32) -> String {
    format!("{INPUT_PIPE_PREFIX}{session_id}")
}

pub fn input_helper_status_path(session_id: u32) -> PathBuf {
    let base = std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("MyKVM")
        .join(format!("input-helper-status-s{session_id}.txt"))
}

pub fn mouse_button_mask(button: MouseButton) -> u64 {
    match button {
        MouseButton::Left => LEFT_BUTTON_MASK,
        MouseButton::Right => RIGHT_BUTTON_MASK,
        MouseButton::Middle => MIDDLE_BUTTON_MASK,
    }
}

pub fn button_from_mask(mask: u64) -> Option<MouseButton> {
    if mask & LEFT_BUTTON_MASK != 0 {
        Some(MouseButton::Left)
    } else if mask & RIGHT_BUTTON_MASK != 0 {
        Some(MouseButton::Right)
    } else if mask & MIDDLE_BUTTON_MASK != 0 {
        Some(MouseButton::Middle)
    } else {
        None
    }
}

pub fn modifier_snapshot_transitions(
    pressed_keys: &[u16],
    authoritative_mask: u8,
) -> Vec<(u16, bool)> {
    let mut transitions = pressed_keys
        .iter()
        .rev()
        .filter_map(|key_code| {
            let bit = modifier_mask_for_key(*key_code)?;
            (authoritative_mask & bit == 0).then_some((*key_code, false))
        })
        .collect::<Vec<_>>();

    for (bit, canonical_key) in [
        (SHIFT_MODIFIER_MASK, 0x10),
        (CONTROL_MODIFIER_MASK, 0x11),
        (ALT_MODIFIER_MASK, 0x12),
        (META_MODIFIER_MASK, 0x5B),
    ] {
        if authoritative_mask & bit != 0
            && !pressed_keys
                .iter()
                .any(|key| modifier_mask_for_key(*key) == Some(bit))
        {
            transitions.push((canonical_key, true));
        }
    }

    transitions
}

pub fn modifier_mask_for_keys(keys: &[u16]) -> u8 {
    keys.iter()
        .filter_map(|key| modifier_mask_for_key(*key))
        .fold(0, |mask, bit| mask | bit)
}

pub fn modifier_mask_for_key(key_code: u16) -> Option<u8> {
    match key_code {
        0x10 | 0xA0 | 0xA1 => Some(SHIFT_MODIFIER_MASK),
        0x11 | 0xA2 | 0xA3 => Some(CONTROL_MODIFIER_MASK),
        0x12 | 0xA4 | 0xA5 => Some(ALT_MODIFIER_MASK),
        0x5B | 0x5C => Some(META_MODIFIER_MASK),
        _ => None,
    }
}

pub fn encode_input_command(command: &InputCommand) -> Result<Vec<u8>, String> {
    let payload = rmp_serde::to_vec_named(command)
        .map_err(|error| format!("encode input command: {error}"))?;
    if payload.len() > u32::MAX as usize {
        return Err("input command is too large".into());
    }

    let mut framed = Vec::with_capacity(4 + payload.len());
    framed.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    framed.extend_from_slice(&payload);
    Ok(framed)
}

pub fn decode_input_command(payload: &[u8]) -> Result<InputCommand, String> {
    rmp_serde::from_slice::<InputCommand>(payload)
        .map_err(|error| format!("decode input command: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_command_frame_round_trips() {
        let command = InputCommand::MouseButton {
            button: MouseButton::Left,
            down: true,
            x: 320,
            y: 240,
        };

        let framed = encode_input_command(&command).expect("encode input command");
        let payload_len = u32::from_le_bytes(
            framed[0..4]
                .try_into()
                .expect("length prefix should be four bytes"),
        ) as usize;

        assert_eq!(payload_len, framed.len() - 4);
        assert_eq!(
            decode_input_command(&framed[4..]).expect("decode input command"),
            command
        );
    }

    #[test]
    fn legacy_mouse_button_event_defaults_missing_coordinates() {
        let event: InputEvent =
            serde_json::from_str(r#"{"type":"mouseButton","button":"left","down":true}"#)
                .expect("decode legacy mouse button");

        assert_eq!(
            event,
            InputEvent::MouseButton {
                button: MouseButton::Left,
                down: true,
                screen_id: String::new(),
                x: None,
                y: None,
                sequence: 0,
            }
        );
    }
}
