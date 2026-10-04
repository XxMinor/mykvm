/// Physical modifiers survive a shortcut's logical key release. Local and
/// remote applications must see balanced keys even when control changes before
/// the user lets go of the shortcut.
#[derive(Clone, Default)]
pub(crate) struct ShortcutKeys {
    physical_modifiers: u16,
    physical_down: [u64; 4],
    local_down: [u64; 4],
    consumed: [u64; 4],
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Modifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub meta: bool,
}

impl ShortcutKeys {
    pub(crate) fn observe(&mut self, key: u16, down: bool) {
        let mask = modifier_bit(key);
        if down {
            self.physical_modifiers |= mask;
            insert(&mut self.physical_down, key);
        } else {
            self.physical_modifiers &= !mask;
            remove(&mut self.physical_down, key);
        }
    }

    pub(crate) fn modifiers(&self) -> Modifiers {
        modifiers(self.physical_modifiers)
    }

    pub(crate) fn local_modifiers(&self) -> Modifiers {
        let mask = [0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0x5B, 0x5C]
            .into_iter()
            .filter(|key| contains(&self.local_down, *key))
            .fold(0, |mask, key| mask | modifier_bit(key));
        modifiers(mask)
    }

    pub(crate) fn deliver_local_down(&mut self, key: u16) {
        insert(&mut self.local_down, key);
    }

    pub(crate) fn physical_modifier_keys(&self) -> Vec<u16> {
        [0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0x5B, 0x5C]
            .into_iter()
            .filter(|key| contains(&self.physical_down, *key))
            .collect()
    }

    pub(crate) fn release_local(&mut self, key: u16) -> bool {
        remove(&mut self.local_down, key)
    }

    /// Keep the trigger key consumed through auto-repeat and its matching up.
    pub(crate) fn consumed_event(&mut self, key: u16, down: bool) -> bool {
        if down {
            contains(&self.consumed, key)
        } else {
            remove(&mut self.consumed, key)
        }
    }

    pub(crate) fn consume(&mut self, key: u16) {
        if contains(&self.physical_down, key) {
            insert(&mut self.consumed, key);
        }
    }

    pub(crate) fn consume_held_keys(&mut self) {
        self.consumed = self.physical_down;
    }

    pub(crate) fn reconcile_physical(&mut self, observed: &Self) {
        self.physical_modifiers = observed.physical_modifiers;
        self.physical_down = observed.physical_down;
        for (consumed, down) in self.consumed.iter_mut().zip(self.physical_down) {
            *consumed &= down;
        }
    }

    pub(crate) fn release_logical_keys(&mut self) -> Vec<u16> {
        let keys = (0..256)
            .filter(|key| canonical_key(*key) == *key && contains(&self.local_down, *key))
            .collect();
        self.local_down = [0; 4];
        keys
    }
}

fn canonical_key(key: u16) -> u16 {
    match key {
        0x10 => 0xA0,
        0x11 => 0xA2,
        0x12 => 0xA4,
        _ => key,
    }
}

fn modifier_bit(key: u16) -> u16 {
    match canonical_key(key) {
        0xA0 => 1,
        0xA1 => 2,
        0xA2 => 4,
        0xA3 => 8,
        0xA4 => 16,
        0xA5 => 32,
        0x5B => 64,
        0x5C => 128,
        _ => 0,
    }
}

fn modifiers(mask: u16) -> Modifiers {
    Modifiers {
        shift: mask & 3 != 0,
        ctrl: mask & 12 != 0,
        alt: mask & 48 != 0,
        meta: mask & 192 != 0,
    }
}

fn contains(keys: &[u64; 4], key: u16) -> bool {
    let key = usize::from(canonical_key(key));
    key < 256 && keys[key / 64] & (1 << (key % 64)) != 0
}

fn insert(keys: &mut [u64; 4], key: u16) {
    let key = usize::from(canonical_key(key));
    if key < 256 {
        keys[key / 64] |= 1 << (key % 64);
    }
}

fn remove(keys: &mut [u64; 4], key: u16) -> bool {
    let present = contains(keys, key);
    let key = usize::from(canonical_key(key));
    if key < 256 {
        keys[key / 64] &= !(1 << (key % 64));
    }
    present
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn held_alt_can_switch_repeatedly_after_both_devices_release_logical_keys() {
        let mut keys = ShortcutKeys::default();
        keys.observe(0xA4, true);
        keys.deliver_local_down(0xA4);
        keys.observe(0x27, true);
        keys.consume(0x27);
        assert_eq!(keys.release_logical_keys(), [0xA4]);
        assert!(keys.modifiers().alt);
        assert!(!keys.local_modifiers().alt);
        assert!(keys.consumed_event(0x27, true)); // Auto-repeat is still one press.
        keys.observe(0x27, false);
        assert!(keys.consumed_event(0x27, false));
        assert!(!keys.consumed_event(0x25, true)); // A different arrow can switch.
        assert!(keys.modifiers().alt);
        keys.observe(0x25, true);
        keys.consume(0x25);
        keys.observe(0xA4, false);
        assert!(!keys.modifiers().alt);
        assert!(keys.consumed_event(0x25, false));
        assert!(!keys.consumed_event(0x25, true)); // Now this is an ordinary arrow.
    }

    #[test]
    fn a_key_started_locally_gets_its_up_even_after_switching_remote() {
        let mut keys = ShortcutKeys::default();
        for key in [0xA2, 0xA4, 0xA0, 0x5B, 0x41] {
            keys.observe(key, true);
            keys.deliver_local_down(key);
            keys.observe(key, false);
            assert!(keys.release_local(key));
            assert!(!keys.release_local(key));
        }
        assert_eq!(keys.modifiers(), Modifiers::default());
    }

    #[test]
    fn modifier_sides_and_generic_aliases_do_not_leave_a_phantom_modifier() {
        let mut keys = ShortcutKeys::default();
        keys.observe(0xA4, true);
        keys.observe(0xA5, true);
        keys.observe(0x12, false);
        assert!(keys.modifiers().alt); // Right Alt is still physically held.
        keys.observe(0xA5, false);
        assert!(!keys.modifiers().alt);
        keys.observe(0x11, true);
        keys.deliver_local_down(0x11);
        keys.release_logical_keys();
        assert!(keys.modifiers().ctrl);
        keys.observe(0xA2, false);
        assert!(!keys.modifiers().ctrl);
    }

    #[test]
    fn recorder_completion_does_not_run_a_binding_while_its_key_is_still_held() {
        let mut keys = ShortcutKeys::default();
        keys.observe(0xA4, true);
        keys.observe(0x27, true);
        keys.deliver_local_down(0xA4);
        keys.deliver_local_down(0x27);
        keys.consume_held_keys();
        assert_eq!(keys.release_logical_keys(), [0x27, 0xA4]);
        assert!(keys.consumed_event(0x27, true));
        keys.observe(0x27, false);
        assert!(keys.consumed_event(0x27, false));
        assert!(!keys.consumed_event(0x27, true));
        assert!(keys.modifiers().alt);
        keys.observe(0xA4, false);
        assert!(!keys.modifiers().alt);
    }
}
