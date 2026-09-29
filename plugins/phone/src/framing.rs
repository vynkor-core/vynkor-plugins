//! Length-prefixed frame reader: the helper writes `u32 big-endian length`
//! followed by that many JPEG bytes, per frame.

use tokio::io::{AsyncRead, AsyncReadExt};

use crate::error::PhoneError;

/// Hard cap on one frame. A 1080p JPEG is ~200 KB; anything near this is garbage.
pub const MAX_FRAME: usize = 8 * 1024 * 1024;

pub fn encode_frame(bytes: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(4 + bytes.len());
    v.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    v.extend_from_slice(bytes);
    v
}

/// SOI at the start and EOI at the end. Cheap sanity check; not a decoder.
pub fn is_jpeg(b: &[u8]) -> bool {
    b.len() >= 4 && b[0] == 0xFF && b[1] == 0xD8 && b[b.len() - 2] == 0xFF && b[b.len() - 1] == 0xD9
}

/// Next frame, `Ok(None)` on a clean EOF at a frame boundary. A partial
/// header, a zero or over-cap length, or a short body is an error.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<Vec<u8>>, PhoneError> {
    let mut hdr = [0u8; 4];
    let mut got = 0;
    while got < 4 {
        let n = r
            .read(&mut hdr[got..])
            .await
            .map_err(|e| PhoneError::Camera(format!("read frame header: {e}")))?;
        if n == 0 {
            return if got == 0 {
                Ok(None)
            } else {
                Err(PhoneError::Camera("truncated frame header".into()))
            };
        }
        got += n;
    }
    let len = u32::from_be_bytes(hdr) as usize;
    if len == 0 {
        return Err(PhoneError::Camera("zero-length frame".into()));
    }
    if len > MAX_FRAME {
        return Err(PhoneError::Camera(format!("frame length {len} exceeds cap {MAX_FRAME}")));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)
        .await
        .map_err(|e| PhoneError::Camera(format!("truncated frame body: {e}")))?;
    Ok(Some(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reads_consecutive_frames_then_clean_eof() {
        let mut data = encode_frame(&[0xFF, 0xD8, 1, 0xFF, 0xD9]);
        data.extend(encode_frame(&[9, 9]));
        let mut r = &data[..];
        assert_eq!(read_frame(&mut r).await.unwrap().unwrap(), vec![0xFF, 0xD8, 1, 0xFF, 0xD9]);
        assert_eq!(read_frame(&mut r).await.unwrap().unwrap(), vec![9, 9]);
        assert_eq!(read_frame(&mut r).await.unwrap(), None);
    }

    #[tokio::test]
    async fn reassembles_a_frame_split_across_reads() {
        let data = encode_frame(&vec![7u8; 5000]);
        let (mut w, mut r) = tokio::io::duplex(64);
        let writer = tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            for chunk in data.chunks(3) {
                w.write_all(chunk).await.unwrap();
            }
        });
        let f = read_frame(&mut r).await.unwrap().unwrap();
        assert_eq!(f.len(), 5000);
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn rejects_zero_oversized_and_truncated() {
        let mut zero = &[0u8, 0, 0, 0][..];
        assert!(matches!(read_frame(&mut zero).await, Err(PhoneError::Camera(_))));

        let huge = ((MAX_FRAME + 1) as u32).to_be_bytes();
        let mut r = &huge[..];
        assert!(matches!(read_frame(&mut r).await, Err(PhoneError::Camera(_))));

        let mut half_hdr = &[0u8, 0][..];
        assert!(matches!(read_frame(&mut half_hdr).await, Err(PhoneError::Camera(_))));

        let mut short = &[0u8, 0, 0, 10, 1, 2, 3][..];
        assert!(matches!(read_frame(&mut short).await, Err(PhoneError::Camera(_))));
    }

    #[test]
    fn jpeg_check() {
        assert!(is_jpeg(&[0xFF, 0xD8, 0, 0xFF, 0xD9]));
        assert!(!is_jpeg(&[0xFF, 0xD8, 0, 0, 0]));
        assert!(!is_jpeg(b"GIF89a"));
        assert!(!is_jpeg(&[]));
        assert!(!is_jpeg(&[0xFF, 0xD8]));
    }
}
