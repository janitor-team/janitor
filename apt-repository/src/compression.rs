//! Compression support for APT repository files.

use crate::Result;
use std::io::{Read, Write};

/// Supported compression formats for APT repository files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    /// No compression.
    None,
    /// Gzip compression.
    Gzip,
    /// Bzip2 compression.
    Bzip2,
}

impl Compression {
    /// Get the file extension for this compression format.
    pub fn extension(&self) -> &'static str {
        match self {
            Compression::None => "",
            Compression::Gzip => ".gz",
            Compression::Bzip2 => ".bz2",
        }
    }

    /// Compress data using this compression format.
    pub fn compress(&self, data: &[u8]) -> Result<Vec<u8>> {
        match self {
            Compression::None => Ok(data.to_vec()),
            Compression::Gzip => {
                let mut compressed = Vec::new();
                let mut encoder =
                    flate2::write::GzEncoder::new(&mut compressed, flate2::Compression::default());
                encoder.write_all(data)?;
                encoder.finish()?;
                Ok(compressed)
            }
            Compression::Bzip2 => {
                let mut compressed = Vec::new();
                let mut encoder =
                    bzip2::write::BzEncoder::new(&mut compressed, bzip2::Compression::default());
                encoder.write_all(data)?;
                encoder.finish()?;
                Ok(compressed)
            }
        }
    }

    /// Decompress data using this compression format.
    pub fn decompress(&self, data: &[u8]) -> Result<Vec<u8>> {
        match self {
            Compression::None => Ok(data.to_vec()),
            Compression::Gzip => {
                let mut decompressed = Vec::new();
                let mut decoder = flate2::read::GzDecoder::new(data);
                decoder.read_to_end(&mut decompressed)?;
                Ok(decompressed)
            }
            Compression::Bzip2 => {
                let mut decompressed = Vec::new();
                let mut decoder = bzip2::read::BzDecoder::new(data);
                decoder.read_to_end(&mut decompressed)?;
                Ok(decompressed)
            }
        }
    }

    /// Get all supported compression formats.
    pub fn all() -> &'static [Compression] {
        &[Compression::None, Compression::Gzip, Compression::Bzip2]
    }
}

impl std::fmt::Display for Compression {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Compression::None => write!(f, "none"),
            Compression::Gzip => write!(f, "gzip"),
            Compression::Bzip2 => write!(f, "bzip2"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compression_extensions() {
        assert_eq!(Compression::None.extension(), "");
        assert_eq!(Compression::Gzip.extension(), ".gz");
        assert_eq!(Compression::Bzip2.extension(), ".bz2");
    }

    #[test]
    fn test_no_compression() -> Result<()> {
        let data = b"hello world";
        let compressed = Compression::None.compress(data)?;
        assert_eq!(compressed, data);

        let decompressed = Compression::None.decompress(&compressed)?;
        assert_eq!(decompressed, data);

        Ok(())
    }

    #[test]
    fn test_gzip_compression() -> Result<()> {
        let data = b"hello world";
        let compressed = Compression::Gzip.compress(data)?;
        assert_ne!(compressed, data);
        assert!(!compressed.is_empty());

        let decompressed = Compression::Gzip.decompress(&compressed)?;
        assert_eq!(decompressed, data);

        Ok(())
    }

    #[test]
    fn test_bzip2_compression() -> Result<()> {
        let data = b"hello world";
        let compressed = Compression::Bzip2.compress(data)?;
        assert_ne!(compressed, data);
        assert!(!compressed.is_empty());

        let decompressed = Compression::Bzip2.decompress(&compressed)?;
        assert_eq!(decompressed, data);

        Ok(())
    }

    #[test]
    fn test_all_compressions() {
        let compressions = Compression::all();
        assert_eq!(compressions.len(), 3);
        assert!(compressions.contains(&Compression::None));
        assert!(compressions.contains(&Compression::Gzip));
        assert!(compressions.contains(&Compression::Bzip2));
    }
}
