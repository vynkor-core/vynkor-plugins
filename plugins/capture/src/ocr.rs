//! Local OCR via `tesseract`, fully offline, argv-only. Accepts either a
//! path to an already-written image (typically a prior
//! `capture_screenshot` result) or inline base64 (written to a temp file
//! first, since `tesseract`'s CLI takes a file path, not stdin bytes).

use base64::Engine;
use serde_json::Value;

use crate::error::CaptureError;
use crate::spawner::Spawner;

pub async fn capture_ocr(spawner: &dyn Spawner, params: &Value) -> Result<Value, CaptureError> {
    let lang = params.get("lang").and_then(Value::as_str).unwrap_or("eng").to_string();
    let path_param = params.get("path").and_then(Value::as_str);
    let base64_param = params.get("base64").and_then(Value::as_str);

    let input_path = match (path_param, base64_param) {
        (Some(p), None) => p.to_string(),
        (None, Some(b64)) => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map_err(|e| CaptureError::BadParams(format!("invalid base64: {e}")))?;
            let tmp = tempfile::NamedTempFile::new()
                .map_err(|e| CaptureError::Backend(format!("temp file: {e}")))?;
            std::fs::write(tmp.path(), &bytes).map_err(|e| CaptureError::Backend(format!("temp file write: {e}")))?;
            let path = tmp.path().to_string_lossy().to_string();
            let _ = tmp.keep().map_err(|e| CaptureError::Backend(format!("temp file keep: {e}")))?;
            path
        }
        (Some(_), Some(_)) => {
            return Err(CaptureError::BadParams("provide exactly one of 'path' or 'base64'".into()))
        }
        (None, None) => {
            return Err(CaptureError::BadParams("provide exactly one of 'path' or 'base64'".into()))
        }
    };

    let (code, stdout) = spawner
        .run_capturing("tesseract", &[input_path, "stdout".to_string(), "-l".to_string(), lang])
        .await
        .map_err(|e| {
            if e.contains("ERR_CAPTURE_PROVIDER_MISSING") {
                CaptureError::NotSupported("ocr")
            } else {
                CaptureError::Backend(e)
            }
        })?;
    if code != 0 {
        return Err(CaptureError::Backend(format!("tesseract exited with code {code}")));
    }
    Ok(serde_json::json!({ "text": stdout }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spawner::FakeSpawner;

    #[tokio::test]
    async fn requires_exactly_one_of_path_or_base64() {
        let sp = FakeSpawner::new();
        let err = capture_ocr(&sp, &serde_json::json!({})).await.unwrap_err();
        assert!(matches!(err, CaptureError::BadParams(_)));

        let err = capture_ocr(&sp, &serde_json::json!({"path": "/a.png", "base64": "eA=="})).await.unwrap_err();
        assert!(matches!(err, CaptureError::BadParams(_)));
    }

    #[tokio::test]
    async fn not_supported_when_tesseract_missing() {
        let sp = FakeSpawner::new(); // no binaries allowed
        let err = capture_ocr(&sp, &serde_json::json!({"path": "/a.png"})).await.unwrap_err();
        assert!(matches!(err, CaptureError::NotSupported("ocr")));
    }

    #[tokio::test]
    async fn returns_stdout_text_on_success() {
        let sp = FakeSpawner::new();
        sp.set_capturing("tesseract", 0, "hello world");
        let v = capture_ocr(&sp, &serde_json::json!({"path": "/a.png"})).await.unwrap();
        assert_eq!(v["text"], "hello world");
    }

    #[tokio::test]
    async fn base64_input_is_written_to_a_temp_file_before_spawn() {
        let sp = FakeSpawner::new();
        sp.set_capturing("tesseract", 0, "x");
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"not really an image");
        let v = capture_ocr(&sp, &serde_json::json!({"base64": b64})).await.unwrap();
        assert_eq!(v["text"], "x");
        let calls = sp.capturing_calls.lock().unwrap().clone();
        assert_eq!(calls[0].0, "tesseract");
        assert!(std::path::Path::new(&calls[0].1[0]).exists());
    }

    #[tokio::test]
    async fn real_tesseract_extracts_text_from_fixture_if_installed() {
        if crate::session::binary_on_path("tesseract") {
            // Fixture: a 100x30 white PNG with black text "OCR" is out of
            // scope to embed here; this test is a placeholder boundary for
            // a real fixture the implementer adds under
            // `plugins/capture/tests/fixtures/ocr_sample.png` (any small
            // PNG with clear black-on-white text works). Skipped
            // automatically when no fixture is present yet.
            let fixture = "tests/fixtures/ocr_sample.png";
            if !std::path::Path::new(fixture).exists() {
                eprintln!("skipping: {fixture} not present");
                return;
            }
            let sp = crate::spawner::RealSpawner;
            let v = capture_ocr(&sp, &serde_json::json!({"path": fixture})).await.unwrap();
            assert!(!v["text"].as_str().unwrap().trim().is_empty());
        } else {
            eprintln!("skipping: tesseract not on PATH");
        }
    }
}
