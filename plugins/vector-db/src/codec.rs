//! f32 little-endian BLOB codec for stored vectors.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodecError {
    /// Byte length is not a multiple of 4.
    BadLength(usize),
}

impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CodecError::BadLength(n) => {
                write!(f, "vector blob length {n} is not a multiple of 4")
            }
        }
    }
}

impl std::error::Error for CodecError {}

pub fn encode(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

pub fn decode(bytes: &[u8]) -> Result<Vec<f32>, CodecError> {
    if bytes.len() % 4 != 0 {
        return Err(CodecError::BadLength(bytes.len()));
    }
    Ok(bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_roundtrip_f32_le() {
        let v = vec![
            0.0,
            -0.0,
            1.0,
            -1.5,
            f32::MIN_POSITIVE,
            f32::MAX,
            0.1,
            1e-30,
        ];
        let bytes = encode(&v);
        assert_eq!(bytes.len(), v.len() * 4);
        // little-endian layout: 1.0f32 == 0x3F800000
        assert_eq!(&bytes[8..12], &[0x00, 0x00, 0x80, 0x3F]);
        let back = decode(&bytes).unwrap();
        assert_eq!(back.len(), v.len());
        for (a, b) in v.iter().zip(back.iter()) {
            assert_eq!(a.to_bits(), b.to_bits(), "bit-exact incl. -0.0");
        }
    }

    #[test]
    fn decode_rejects_bad_length() {
        assert_eq!(decode(&[1, 2, 3]), Err(CodecError::BadLength(3)));
        assert_eq!(decode(&[0; 5]), Err(CodecError::BadLength(5)));
        assert_eq!(decode(&[]).unwrap(), Vec::<f32>::new());
    }
}
