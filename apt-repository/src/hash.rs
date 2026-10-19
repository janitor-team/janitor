//! Cryptographic hashing support for APT repositories.

use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::collections::HashMap;
use std::fmt;
use std::io::Write;

/// Supported hash algorithms for APT repositories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HashAlgorithm {
    /// MD5 hash algorithm.
    Md5,
    /// SHA-1 hash algorithm.
    Sha1,
    /// SHA-256 hash algorithm.
    Sha256,
    /// SHA-512 hash algorithm.
    Sha512,
}

impl HashAlgorithm {
    /// Get the string representation used in Release files.
    pub fn as_str(&self) -> &'static str {
        match self {
            HashAlgorithm::Md5 => "MD5Sum",
            HashAlgorithm::Sha1 => "SHA1",
            HashAlgorithm::Sha256 => "SHA256",
            HashAlgorithm::Sha512 => "SHA512",
        }
    }

    /// Get all supported hash algorithms.
    pub fn all() -> &'static [HashAlgorithm] {
        &[
            HashAlgorithm::Md5,
            HashAlgorithm::Sha1,
            HashAlgorithm::Sha256,
            HashAlgorithm::Sha512,
        ]
    }
}

impl fmt::Display for HashAlgorithm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// A set of hashes for a single file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HashSet {
    hashes: HashMap<HashAlgorithm, String>,
}

impl HashSet {
    /// Create a new empty hash set.
    pub fn new() -> Self {
        Self {
            hashes: HashMap::new(),
        }
    }

    /// Add a hash to the set.
    pub fn insert(&mut self, algorithm: HashAlgorithm, hash: String) {
        self.hashes.insert(algorithm, hash);
    }

    /// Get a hash by algorithm.
    pub fn get(&self, algorithm: &HashAlgorithm) -> Option<&str> {
        self.hashes.get(algorithm).map(|s| s.as_str())
    }

    /// Get all hashes as an iterator.
    pub fn iter(&self) -> impl Iterator<Item = (&HashAlgorithm, &str)> {
        self.hashes.iter().map(|(k, v)| (k, v.as_str()))
    }

    /// Check if the hash set is empty.
    pub fn is_empty(&self) -> bool {
        self.hashes.is_empty()
    }

    /// Get the number of hashes in the set.
    pub fn len(&self) -> usize {
        self.hashes.len()
    }
}

impl Default for HashSet {
    fn default() -> Self {
        Self::new()
    }
}

/// A file with its associated hashes and size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HashedFile {
    /// The relative path of the file within the repository.
    pub path: String,
    /// The size of the file in bytes.
    pub size: u64,
    /// The hashes of the file.
    pub hashes: HashSet,
}

impl HashedFile {
    /// Create a new hashed file.
    pub fn new<S: Into<String>>(path: S, size: u64) -> Self {
        Self {
            path: path.into(),
            size,
            hashes: HashSet::new(),
        }
    }

    /// Add a hash to the file.
    pub fn add_hash(&mut self, algorithm: HashAlgorithm, hash: String) {
        self.hashes.insert(algorithm, hash);
    }

    /// Get a hash by algorithm.
    pub fn get_hash(&self, algorithm: &HashAlgorithm) -> Option<&str> {
        self.hashes.get(algorithm)
    }
}

/// A multi-hash calculator that can compute multiple hash algorithms simultaneously.
pub struct MultiHasher {
    md5: Option<md5::Context>,
    sha1: Option<sha1::Sha1>,
    sha256: Option<sha2::Sha256>,
    sha512: Option<sha2::Sha512>,
    size: u64,
}

impl MultiHasher {
    /// Create a new multi-hasher with the specified algorithms.
    pub fn new(algorithms: &[HashAlgorithm]) -> Self {
        let mut hasher = Self {
            md5: None,
            sha1: None,
            sha256: None,
            sha512: None,
            size: 0,
        };

        for &algorithm in algorithms {
            match algorithm {
                HashAlgorithm::Md5 => hasher.md5 = Some(md5::Context::new()),
                HashAlgorithm::Sha1 => hasher.sha1 = Some(sha1::Sha1::new()),
                HashAlgorithm::Sha256 => {
                    use sha2::Digest;
                    hasher.sha256 = Some(sha2::Sha256::new());
                }
                HashAlgorithm::Sha512 => {
                    use sha2::Digest;
                    hasher.sha512 = Some(sha2::Sha512::new());
                }
            }
        }

        hasher
    }

    /// Update the hashes with the given data.
    pub fn update(&mut self, data: &[u8]) {
        self.size += data.len() as u64;

        if let Some(ref mut hasher) = self.md5 {
            hasher.consume(data);
        }
        if let Some(ref mut hasher) = self.sha1 {
            use sha1::Digest;
            hasher.update(data);
        }
        if let Some(ref mut hasher) = self.sha256 {
            use sha2::Digest;
            hasher.update(data);
        }
        if let Some(ref mut hasher) = self.sha512 {
            use sha2::Digest;
            hasher.update(data);
        }
    }

    /// Finalize the hashes and return the results.
    pub fn finalize(self) -> (u64, HashSet) {
        let mut hash_set = HashSet::new();

        if let Some(hasher) = self.md5 {
            let hash = format!("{:x}", hasher.compute());
            hash_set.insert(HashAlgorithm::Md5, hash);
        }
        if let Some(hasher) = self.sha1 {
            use sha1::Digest;
            let hash = format!("{:x}", hasher.finalize());
            hash_set.insert(HashAlgorithm::Sha1, hash);
        }
        if let Some(hasher) = self.sha256 {
            use sha2::Digest;
            let hash = format!("{:x}", hasher.finalize());
            hash_set.insert(HashAlgorithm::Sha256, hash);
        }
        if let Some(hasher) = self.sha512 {
            use sha2::Digest;
            let hash = format!("{:x}", hasher.finalize());
            hash_set.insert(HashAlgorithm::Sha512, hash);
        }

        (self.size, hash_set)
    }

    /// Get the current size.
    pub fn size(&self) -> u64 {
        self.size
    }
}

impl Write for MultiHasher {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Hash data with the specified algorithms.
pub fn hash_data(data: &[u8], algorithms: &[HashAlgorithm]) -> (u64, HashSet) {
    let mut hasher = MultiHasher::new(algorithms);
    hasher.update(data);
    hasher.finalize()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hash_algorithm_str() {
        assert_eq!(HashAlgorithm::Md5.as_str(), "MD5Sum");
        assert_eq!(HashAlgorithm::Sha1.as_str(), "SHA1");
        assert_eq!(HashAlgorithm::Sha256.as_str(), "SHA256");
        assert_eq!(HashAlgorithm::Sha512.as_str(), "SHA512");
    }

    #[test]
    fn test_hash_set() {
        let mut hash_set = HashSet::new();
        assert!(hash_set.is_empty());
        assert_eq!(hash_set.len(), 0);

        hash_set.insert(HashAlgorithm::Md5, "abc123".to_string());
        assert!(!hash_set.is_empty());
        assert_eq!(hash_set.len(), 1);
        assert_eq!(hash_set.get(&HashAlgorithm::Md5), Some("abc123"));
        assert_eq!(hash_set.get(&HashAlgorithm::Sha1), None);
    }

    #[test]
    fn test_multi_hasher() {
        let data = b"hello world";
        let algorithms = &[HashAlgorithm::Md5, HashAlgorithm::Sha256];

        let mut hasher = MultiHasher::new(algorithms);
        hasher.update(data);
        let (size, hashes) = hasher.finalize();

        assert_eq!(size, data.len() as u64);
        assert_eq!(hashes.len(), 2);
        assert!(hashes.get(&HashAlgorithm::Md5).is_some());
        assert!(hashes.get(&HashAlgorithm::Sha256).is_some());
        assert!(hashes.get(&HashAlgorithm::Sha1).is_none());
    }

    #[test]
    fn test_hash_data() {
        let data = b"test data";
        let algorithms = &[HashAlgorithm::Md5];

        let (size, hashes) = hash_data(data, algorithms);

        assert_eq!(size, data.len() as u64);
        assert_eq!(hashes.len(), 1);

        // Verify the MD5 hash
        let expected_md5 = format!("{:x}", md5::compute(data));
        assert_eq!(hashes.get(&HashAlgorithm::Md5), Some(expected_md5.as_str()));
    }
}
