use std::collections::HashMap;
use std::sync::RwLock;

use super::posting::{PostingEntry, PostingList};

pub trait PostingStore: Send + Sync {
    fn get(&self, head_id: u32) -> Option<PostingList>;
    fn put(&self, head_id: u32, posting: PostingList);
    fn append(&self, head_id: u32, entries: &[PostingEntry]);
    fn delete_posting(&self, head_id: u32);
    fn get_size(&self, head_id: u32) -> usize;
    fn get_multi(&self, head_ids: &[u32]) -> Vec<(u32, PostingList)>;
}

pub struct MemoryPostingStore {
    data: RwLock<HashMap<u32, PostingList>>,
}

impl MemoryPostingStore {
    pub fn new() -> Self {
        Self {
            data: RwLock::new(HashMap::new()),
        }
    }
}

impl Default for MemoryPostingStore {
    fn default() -> Self {
        Self::new()
    }
}

impl PostingStore for MemoryPostingStore {
    fn get(&self, head_id: u32) -> Option<PostingList> {
        self.data.read().unwrap().get(&head_id).cloned()
    }

    fn put(&self, head_id: u32, posting: PostingList) {
        self.data.write().unwrap().insert(head_id, posting);
    }

    fn append(&self, head_id: u32, entries: &[PostingEntry]) {
        let mut map = self.data.write().unwrap();
        let pl = map.entry(head_id).or_default();
        pl.extend(entries.iter().cloned());
    }

    fn delete_posting(&self, head_id: u32) {
        self.data.write().unwrap().remove(&head_id);
    }

    fn get_size(&self, head_id: u32) -> usize {
        self.data
            .read()
            .unwrap()
            .get(&head_id)
            .map_or(0, |pl| pl.len())
    }

    fn get_multi(&self, head_ids: &[u32]) -> Vec<(u32, PostingList)> {
        let map = self.data.read().unwrap();
        head_ids
            .iter()
            .filter_map(|&id| map.get(&id).map(|pl| (id, pl.clone())))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(id: u64) -> PostingEntry {
        PostingEntry {
            vector_id: id,
            version: 1,
            vector: vec![id as f32; 2],
        }
    }

    #[test]
    fn test_put_get() {
        let store = MemoryPostingStore::new();
        let pl = PostingList {
            entries: vec![make_entry(1), make_entry(2)],
        };
        store.put(0, pl.clone());
        let got = store.get(0).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got.entries[0].vector_id, 1);
    }

    #[test]
    fn test_get_missing() {
        let store = MemoryPostingStore::new();
        assert!(store.get(99).is_none());
    }

    #[test]
    fn test_append() {
        let store = MemoryPostingStore::new();
        store.append(0, &[make_entry(1)]);
        store.append(0, &[make_entry(2), make_entry(3)]);
        assert_eq!(store.get_size(0), 3);
    }

    #[test]
    fn test_delete() {
        let store = MemoryPostingStore::new();
        store.put(0, PostingList { entries: vec![make_entry(1)] });
        store.delete_posting(0);
        assert!(store.get(0).is_none());
        assert_eq!(store.get_size(0), 0);
    }

    #[test]
    fn test_get_multi() {
        let store = MemoryPostingStore::new();
        store.put(1, PostingList { entries: vec![make_entry(10)] });
        store.put(3, PostingList { entries: vec![make_entry(30)] });

        let results = store.get_multi(&[1, 2, 3]);
        assert_eq!(results.len(), 2);
        let ids: Vec<u32> = results.iter().map(|(id, _)| *id).collect();
        assert!(ids.contains(&1));
        assert!(ids.contains(&3));
    }
}
