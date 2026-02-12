use std::sync::atomic::{AtomicU8, Ordering};

const DELETED_BIT: u8 = 0x80;
const VERSION_MASK: u8 = 0x7F;

pub struct VersionMap {
    versions: Vec<AtomicU8>,
}

impl VersionMap {
    pub fn new(capacity: usize) -> Self {
        let mut versions = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            versions.push(AtomicU8::new(0));
        }
        Self { versions }
    }

    pub fn ensure_capacity(&mut self, vid: u64) {
        let idx = vid as usize;
        if idx >= self.versions.len() {
            self.versions.resize_with(idx + 1, || AtomicU8::new(0));
        }
    }

    pub fn initialize(&self, vid: u64) {
        let idx = vid as usize;
        if idx < self.versions.len() {
            self.versions[idx].store(1, Ordering::Release);
        }
    }

    pub fn get_version(&self, vid: u64) -> u8 {
        let idx = vid as usize;
        if idx >= self.versions.len() {
            return 0;
        }
        self.versions[idx].load(Ordering::Acquire) & VERSION_MASK
    }

    pub fn is_deleted(&self, vid: u64) -> bool {
        let idx = vid as usize;
        if idx >= self.versions.len() {
            return false;
        }
        self.versions[idx].load(Ordering::Acquire) & DELETED_BIT != 0
    }

    pub fn mark_deleted(&self, vid: u64) {
        let idx = vid as usize;
        if idx < self.versions.len() {
            self.versions[idx].fetch_or(DELETED_BIT, Ordering::Release);
        }
    }

    pub fn increment_version(&self, vid: u64) -> Option<u8> {
        let idx = vid as usize;
        if idx >= self.versions.len() {
            return None;
        }
        loop {
            let current = self.versions[idx].load(Ordering::Acquire);
            if current & DELETED_BIT != 0 {
                return None;
            }
            let ver = current & VERSION_MASK;
            let new_ver = if ver == VERSION_MASK { 1 } else { ver + 1 };
            match self.versions[idx].compare_exchange(
                current,
                new_ver,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(new_ver),
                Err(_) => continue,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn test_basic_operations() {
        let vm = VersionMap::new(10);
        assert_eq!(vm.get_version(0), 0);
        assert!(!vm.is_deleted(0));

        vm.initialize(0);
        assert_eq!(vm.get_version(0), 1);
        assert!(!vm.is_deleted(0));

        let v = vm.increment_version(0);
        assert_eq!(v, Some(2));
        assert_eq!(vm.get_version(0), 2);

        vm.mark_deleted(0);
        assert!(vm.is_deleted(0));
        assert_eq!(vm.increment_version(0), None);
    }

    #[test]
    fn test_ensure_capacity() {
        let mut vm = VersionMap::new(2);
        assert_eq!(vm.get_version(5), 0);
        vm.ensure_capacity(5);
        vm.initialize(5);
        assert_eq!(vm.get_version(5), 1);
    }

    #[test]
    fn test_version_wraps() {
        let vm = VersionMap::new(1);
        vm.initialize(0);
        for _ in 1..127 {
            vm.increment_version(0);
        }
        assert_eq!(vm.get_version(0), 127);
        let v = vm.increment_version(0);
        assert_eq!(v, Some(1));
    }

    #[test]
    fn test_concurrent_increment() {
        let vm = VersionMap::new(1);
        vm.initialize(0);
        let vm = Arc::new(vm);

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let vm = Arc::clone(&vm);
                thread::spawn(move || {
                    for _ in 0..100 {
                        vm.increment_version(0);
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        let ver = vm.get_version(0);
        // 1 initial + 800 increments, wrapping at 127
        let expected = ((1u32 + 800) % 127) as u8;
        let expected = if expected == 0 { 127 } else { expected };
        // With wrapping: (801 % 127) = 801 - 6*127 = 801 - 762 = 39
        assert_eq!(ver, expected);
    }
}
