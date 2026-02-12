use serde::{Serialize, Deserialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PostingEntry {
    pub vector_id: u64,
    pub version: u8,
    pub vector: Vec<f32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PostingList {
    pub entries: Vec<PostingEntry>,
}

impl PostingList {
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn push(&mut self, entry: PostingEntry) {
        self.entries.push(entry);
    }

    pub fn extend(&mut self, entries: impl IntoIterator<Item = PostingEntry>) {
        self.entries.extend(entries);
    }

    pub fn iter(&self) -> std::slice::Iter<'_, PostingEntry> {
        self.entries.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_posting_list_ops() {
        let mut pl = PostingList::default();
        assert!(pl.is_empty());

        pl.push(PostingEntry {
            vector_id: 1,
            version: 1,
            vector: vec![1.0, 2.0],
        });
        assert_eq!(pl.len(), 1);

        pl.extend(vec![
            PostingEntry { vector_id: 2, version: 1, vector: vec![3.0, 4.0] },
            PostingEntry { vector_id: 3, version: 1, vector: vec![5.0, 6.0] },
        ]);
        assert_eq!(pl.len(), 3);

        let ids: Vec<u64> = pl.iter().map(|e| e.vector_id).collect();
        assert_eq!(ids, vec![1, 2, 3]);
    }

    #[test]
    fn test_bincode_roundtrip() {
        let pl = PostingList {
            entries: vec![
                PostingEntry { vector_id: 42, version: 3, vector: vec![1.0, 2.0, 3.0] },
                PostingEntry { vector_id: 99, version: 7, vector: vec![4.0, 5.0, 6.0] },
            ],
        };

        let bytes = bincode::serialize(&pl).unwrap();
        let decoded: PostingList = bincode::deserialize(&bytes).unwrap();
        assert_eq!(pl, decoded);
    }

    #[test]
    fn test_entry_bincode_roundtrip() {
        let entry = PostingEntry {
            vector_id: 123,
            version: 42,
            vector: vec![0.1, 0.2, 0.3, 0.4],
        };
        let bytes = bincode::serialize(&entry).unwrap();
        let decoded: PostingEntry = bincode::deserialize(&bytes).unwrap();
        assert_eq!(entry, decoded);
    }
}
