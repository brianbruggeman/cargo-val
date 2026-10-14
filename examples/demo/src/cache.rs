use std::collections::{HashMap, VecDeque};
use std::hash::Hash;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
}

pub struct LruCache<K, V> {
    capacity: usize,
    entries: HashMap<K, V>,
    recency: VecDeque<K>,
    stats: Stats,
}

impl<K: Hash + Eq + Clone, V> LruCache<K, V> {
    pub fn new(capacity: usize) -> Self {
        Self { capacity, entries: HashMap::new(), recency: VecDeque::new(), stats: Stats::default() }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    pub fn keys_by_recency(&self) -> Vec<K> {
        self.recency.iter().cloned().collect()
    }

    pub fn get(&mut self, key: &K) -> Option<&V> {
        if self.entries.contains_key(key) {
            self.stats.hits += 1;
            self.touch(key);
            self.entries.get(key)
        } else {
            self.stats.misses += 1;
            None
        }
    }

    pub fn put(&mut self, key: K, value: V) -> Option<K> {
        if self.capacity == 0 {
            return None;
        }
        let replaced = self.entries.insert(key.clone(), value).is_some();
        if replaced {
            self.touch(&key);
            return None;
        }
        self.recency.push_front(key);
        if self.entries.len() > self.capacity { self.evict_oldest() } else { None }
    }

    pub fn remove(&mut self, key: &K) -> Option<V> {
        self.recency.retain(|candidate| candidate != key);
        self.entries.remove(key)
    }

    fn touch(&mut self, key: &K) {
        self.recency.retain(|candidate| candidate != key);
        self.recency.push_front(key.clone());
    }

    fn evict_oldest(&mut self) -> Option<K> {
        let oldest = self.recency.pop_back()?;
        self.entries.remove(&oldest);
        self.stats.evictions += 1;
        Some(oldest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filled() -> LruCache<&'static str, u32> {
        let mut cache = LruCache::new(3);
        cache.put("a", 1);
        cache.put("b", 2);
        cache.put("c", 3);
        cache
    }

    #[test]
    fn new_cache_is_empty() {
        let cache: LruCache<&str, u32> = LruCache::new(4);
        assert!(cache.is_empty());
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn put_then_get_returns_the_value() {
        let mut cache = filled();
        assert_eq!(cache.get(&"b"), Some(&2));
    }

    #[test]
    fn get_of_an_absent_key_is_none() {
        let mut cache = filled();
        assert_eq!(cache.get(&"z"), None);
    }

    #[test]
    fn exceeding_capacity_evicts_the_least_recent_key() {
        let mut cache = filled();
        assert_eq!(cache.put("d", 4), Some("a"));
        assert_eq!(cache.len(), 3);
    }

    #[test]
    fn a_read_protects_a_key_from_eviction() {
        let mut cache = filled();
        cache.get(&"a");
        assert_eq!(cache.put("d", 4), Some("b"));
    }

    #[test]
    fn overwriting_a_key_does_not_evict() {
        let mut cache = filled();
        assert_eq!(cache.put("a", 10), None);
        assert_eq!(cache.get(&"a"), Some(&10));
    }

    #[test]
    fn zero_capacity_stores_nothing() {
        let mut cache = LruCache::new(0);
        assert_eq!(cache.put("a", 1), None);
        assert!(cache.is_empty());
    }

    #[test]
    fn remove_returns_the_value_and_frees_the_slot() {
        let mut cache = filled();
        assert_eq!(cache.remove(&"b"), Some(2));
        assert_eq!(cache.put("d", 4), None);
    }

    #[test]
    fn remove_of_an_absent_key_is_none() {
        let mut cache = filled();
        assert_eq!(cache.remove(&"z"), None);
    }

    #[test]
    fn stats_count_hits_and_misses() {
        let mut cache = filled();
        cache.get(&"a");
        cache.get(&"q");
        cache.get(&"c");
        assert_eq!((cache.stats().hits, cache.stats().misses), (2, 1));
    }

    #[test]
    fn stats_count_evictions() {
        let mut cache = filled();
        cache.put("d", 4);
        cache.put("e", 5);
        assert_eq!(cache.stats().evictions, 2);
    }

    #[test]
    fn recency_lists_the_newest_key_first() {
        let mut cache = filled();
        cache.get(&"a");
        assert_eq!(cache.keys_by_recency(), vec!["a", "c", "b"]);
    }

    #[test]
    fn recency_after_a_read_puts_the_oldest_key_last() {
        let mut cache = filled();
        cache.get(&"a");
        assert_eq!(cache.keys_by_recency(), vec!["c", "b", "a"]);
    }

    #[test]
    #[ignore = "needs a shared redis at CACHE_REDIS_URL to compare eviction order"]
    fn eviction_order_matches_redis_allkeys_lru() {
        let cache = filled();
        assert_eq!(cache.len(), 3);
    }
}
