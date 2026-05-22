use std::fs;
use std::io::{BufReader, Read};
use std::path::Path;

use crate::errors::GitError;
use crate::internal::object::types::ObjectType;
use crate::internal::pack::{Pack, utils};

/// Pack 文件统计信息
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct PackStats {
    /// 总对象数
    pub total: usize,
    /// commit 对象数
    pub commits: usize,
    /// tree 对象数
    pub trees: usize,
    /// blob 对象数
    pub blobs: usize,
    /// tag 对象数
    pub tags: usize,
    /// delta 对象数（包括 OffsetDelta、OffsetZstdelta、HashDelta）
    pub deltas: usize,
}

impl PackStats {
    /// 创建一个空的 PackStats
    pub fn new() -> Self {
        PackStats {
            total: 0,
            commits: 0,
            trees: 0,
            blobs: 0,
            tags: 0,
            deltas: 0,
        }
    }
}

/// 分析 pack 文件，返回对象数量和类型分布
///
/// 该函数复用了 `Pack::check_header` 和 `utils::read_type_and_varint_size` 等已有逻辑，
/// 但不进行完整的解码，只读取对象头部信息进行统计。
pub fn analyze_pack(path: impl AsRef<Path>) -> Result<PackStats, GitError> {
    let path = path.as_ref();

    // 检查文件是否存在
    if !path.exists() {
        return Err(GitError::InvalidPackFile(format!(
            "File not found: {:?}",
            path
        )));
    }

    // 检查文件是否太小
    let metadata = fs::metadata(path)?;
    if metadata.len() < 12 {
        return Err(GitError::InvalidPackFile("Pack file too small".to_string()));
    }

    let file = fs::File::open(path)?;
    let mut reader = BufReader::new(file);

    // 使用已有的 check_header 函数验证头部
    let (object_num, _) = Pack::check_header(&mut reader)?;

    let mut stats = PackStats::new();
    let mut offset: usize = 12; // 跳过头部
    let hash_size = crate::hash::get_hash_kind().size();

    for _ in 0..object_num {
        // 使用已有的 read_type_and_varint_size 读取类型和大小
        let (type_bits, size) = utils::read_type_and_varint_size(&mut reader, &mut offset)?;

        let obj_type = ObjectType::from_pack_type_u8(type_bits)?;

        // 更新统计
        stats.total += 1;
        match obj_type {
            ObjectType::Commit => stats.commits += 1,
            ObjectType::Tree => stats.trees += 1,
            ObjectType::Blob => stats.blobs += 1,
            ObjectType::Tag => stats.tags += 1,
            ObjectType::OffsetDelta | ObjectType::OffsetZstdelta | ObjectType::HashDelta => {
                stats.deltas += 1;
                // delta 对象需要额外处理：REF_DELTA 需要读取 20/32 字节的 hash，OFS_DELTA 需要读取偏移量
                if obj_type == ObjectType::HashDelta {
                    // REF_DELTA: 跳过 hash
                    let mut hash_buf = vec![0u8; hash_size];
                    reader.read_exact(&mut hash_buf)?;
                    offset += hash_size;
                } else {
                    // OFFSET_DELTA: 读取可变长度偏移
                    let (_delta_offset, bytes) = utils::read_offset_encoding(&mut reader)?;
                    offset += bytes;
                }
            }
            _ => {
                return Err(GitError::InvalidPackFile(format!(
                    "Unknown object type: {}",
                    obj_type
                )));
            }
        }

        // 跳过压缩数据（只读取大小，不解压）
        // 为了正确跳过压缩数据，我们需要使用 CountingReader 来跟踪读取的字节数
        let mut counting_reader = crate::utils::CountingReader::new(&mut reader);
        let mut deflate = flate2::bufread::ZlibDecoder::new(&mut counting_reader);
        let mut buf = Vec::with_capacity(size);
        if let Err(e) = deflate.read_to_end(&mut buf) {
            return Err(GitError::InvalidPackFile(format!(
                "Decompression error: {}",
                e
            )));
        }
        offset += counting_reader.bytes_read as usize;
    }

    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::ZlibEncoder;
    use std::env;
    use std::fs;
    use std::io::Write;
    use std::path::PathBuf;
    use tempfile::tempdir;

    // 创建一个真实的、有效的 pack 文件用于测试
    fn create_valid_test_pack() -> Vec<u8> {
        let mut data = Vec::new();

        // 1. Header: "PACK" + version 2 + 2 objects
        data.extend_from_slice(b"PACK");
        data.extend_from_slice(&2u32.to_be_bytes());
        data.extend_from_slice(&2u32.to_be_bytes()); // 2 objects

        // 对象 1: 一个 commit 对象
        // 原始内容 "commit 1\n"
        let content1 = b"commit 1\n";
        let compressed1 = compress_zlib(content1);

        // 对象头部: type=1 (commit), size=9
        // 第一字节: (type << 4) | (size & 0x0F)
        // type=1, size=9 (0x09)
        data.push(0x19); // 0001 1001: type=1, size低4位=9, MSB=0 (无续字节)
        data.extend_from_slice(&compressed1);

        // 对象 2: 一个 tree 对象
        // 原始内容 "tree 1\n"
        let content2 = b"tree 1\n";
        let compressed2 = compress_zlib(content2);

        // 对象头部: type=2 (tree), size=7
        // type=2, size=7 (0x07)
        data.push(0x27); // 0010 0111: type=2, size低4位=7, MSB=0
        data.extend_from_slice(&compressed2);

        data
    }

    // 创建一个包含 delta 对象的 pack 文件
    fn create_delta_test_pack() -> Vec<u8> {
        let mut data = Vec::new();

        // Header: "PACK" + version 2 + 3 objects
        data.extend_from_slice(b"PACK");
        data.extend_from_slice(&2u32.to_be_bytes());
        data.extend_from_slice(&3u32.to_be_bytes());

        // 对象 1: commit (base)
        let content1 = b"commit base\n";
        let compressed1 = compress_zlib(content1);
        data.push(0x1c); // type=1, size=12
        data.extend_from_slice(&compressed1);

        // 对象 2: REF_DELTA (type=7)
        // 头部: type=7, size=small
        data.push(0x77); // 0111 0111: type=7, size低4位=7
        // REF_DELTA 需要 20 字节的 SHA-1 (这里用假的)
        data.extend_from_slice(&[0u8; 20]);
        // 压缩的 delta 数据
        let delta_content = b"delta data";
        let compressed_delta = compress_zlib(delta_content);
        data.extend_from_slice(&compressed_delta);

        // 对象 3: OFS_DELTA (type=6)
        data.push(0x66); // 0110 0110: type=6, size低4位=6
        // OFS_DELTA 需要可变长度偏移
        data.push(0x80); // 偏移量编码
        let compressed_delta2 = compress_zlib(b"more delta");
        data.extend_from_slice(&compressed_delta2);

        data
    }

    // 辅助函数：压缩数据
    fn compress_zlib(data: &[u8]) -> Vec<u8> {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn test_analyze_valid_pack_sha1() {
        let mut source = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        source.push("tests/data/packs/small-sha1.pack");

        let result = analyze_pack(&source);
        assert!(result.is_ok(), "Failed to analyze pack: {:?}", result.err());

        let stats = result.unwrap();
        assert!(stats.total > 0, "Expected at least 1 object");
        println!("Pack statistics (small-sha1): {:?}", stats);
    }

    #[test]
    fn test_analyze_valid_pack_sha256() {
        let mut source = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        source.push("tests/data/packs/small-sha256.pack");

        let result = analyze_pack(&source);
        assert!(result.is_ok(), "Failed to analyze pack: {:?}", result.err());

        let stats = result.unwrap();
        assert!(stats.total > 0, "Expected at least 1 object");
        println!("Pack statistics (small-sha256): {:?}", stats);
    }

    #[test]
    fn test_analyze_pack_with_ref_delta_sha1() {
        let mut source = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        source.push("tests/data/packs/ref-delta-sha1.pack");

        let result = analyze_pack(&source);
        assert!(
            result.is_ok(),
            "Failed to analyze delta pack: {:?}",
            result.err()
        );

        let stats = result.unwrap();
        assert!(stats.total > 0, "Expected at least 1 object");
        println!("Delta pack statistics (ref-delta-sha1): {:?}", stats);
    }

    #[test]
    fn test_analyze_pack_with_ref_delta_sha256() {
        let mut source = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        source.push("tests/data/packs/ref-delta-sha256.pack");

        let result = analyze_pack(&source);
        assert!(
            result.is_ok(),
            "Failed to analyze delta pack: {:?}",
            result.err()
        );

        let stats = result.unwrap();
        assert!(stats.total > 0, "Expected at least 1 object");
        println!("Delta pack statistics (ref-delta-sha256): {:?}", stats);
    }

    #[test]
    fn test_file_not_found() {
        let result = analyze_pack("non_existent_file_xyz123.pack");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("File not found"));
    }

    #[test]
    fn test_invalid_pack() {
        let dir = tempdir().unwrap();
        let invalid_path = dir.path().join("invalid.pack");

        // 写入无效数据
        fs::write(&invalid_path, b"INVALID DATA").unwrap();

        let result = analyze_pack(&invalid_path);
        assert!(result.is_err());
        // 检查是否为 InvalidPackHeader 错误
        let err = result.unwrap_err();
        assert!(
            matches!(err, GitError::InvalidPackHeader(_)),
            "Expected InvalidPackHeader error, got: {}",
            err
        );
    }

    #[test]
    fn test_too_small_file() {
        let dir = tempdir().unwrap();
        let small_path = dir.path().join("small.pack");

        // 写入太小的文件
        fs::write(&small_path, b"PACK").unwrap();

        let result = analyze_pack(&small_path);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("too small"));
    }

    #[test]
    fn test_invalid_version() {
        let dir = tempdir().unwrap();
        let pack_path = dir.path().join("test.pack");

        let mut data = Vec::new();
        data.extend_from_slice(b"PACK");
        data.extend_from_slice(&3u32.to_be_bytes()); // 版本 3（不支持）
        data.extend_from_slice(&1u32.to_be_bytes());

        fs::write(&pack_path, data).unwrap();

        let result = analyze_pack(&pack_path);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Version Number is 3")
        );
    }
}
