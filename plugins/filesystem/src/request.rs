//! Request parsing for the `filesystem` plugin: turn raw `params_json` into
//! typed request values at the boundary, so the interior never re-validates.
//! Errors name the offending field.

use base64::Engine as _;

#[derive(Debug)]
pub enum Request {
    List(ListParams),
    Read(ReadParams),
    Write(WriteParams),
    Delete(DeleteParams),
    Mkdir(MkdirParams),
    Rename(RenameParams),
    Move(MoveParams),
}

#[derive(Debug)]
pub struct ListParams {
    pub path: String,
    pub include_hidden: bool,
}

#[derive(Debug)]
pub struct ReadParams {
    pub path: String,
    pub offset: u64,
    pub max_bytes: Option<u64>,
}

#[derive(Debug)]
pub struct WriteParams {
    pub path: String,
    pub bytes: Vec<u8>,
    pub create_parents: bool,
}

#[derive(Debug)]
pub struct DeleteParams {
    pub path: String,
    pub to_trash: bool,
}

#[derive(Debug)]
pub struct MkdirParams {
    pub path: String,
    pub parents: bool,
}

#[derive(Debug)]
pub struct RenameParams {
    pub from: String,
    pub to: String,
    pub overwrite: bool,
}

#[derive(Debug)]
pub struct MoveParams {
    pub from: String,
    pub to: String,
    pub overwrite: bool,
}

#[derive(serde::Deserialize)]
struct ListRaw {
    path: String,
    #[serde(default)]
    include_hidden: bool,
}

#[derive(serde::Deserialize)]
struct ReadRaw {
    path: String,
    #[serde(default)]
    offset: u64,
    max_bytes: Option<u64>,
}

#[derive(serde::Deserialize)]
struct WriteRaw {
    path: String,
    text: Option<String>,
    content_base64: Option<String>,
    #[serde(default)]
    create_parents: bool,
}

#[derive(serde::Deserialize)]
struct DeleteRaw {
    path: String,
    to_trash: Option<bool>,
}

#[derive(serde::Deserialize)]
struct MkdirRaw {
    path: String,
    #[serde(default)]
    parents: bool,
}

#[derive(serde::Deserialize)]
struct RenameRaw {
    from: String,
    to: String,
    #[serde(default)]
    overwrite: bool,
}

#[derive(serde::Deserialize)]
struct MoveRaw {
    from: String,
    to: String,
    #[serde(default)]
    overwrite: bool,
}

fn require_nonempty_path(path: String, action: &str) -> Result<String, String> {
    if path.is_empty() {
        Err(format!(
            "ERR_FILES_BAD_PARAMS: `path` must be a non-empty string ({action})"
        ))
    } else {
        Ok(path)
    }
}

pub fn parse_request(action: &str, params_json: &[u8]) -> Result<Request, String> {
    match action {
        "fs_list" => {
            let p: ListRaw = serde_json::from_slice(params_json).map_err(|e| {
                format!("ERR_FILES_BAD_PARAMS: invalid params for fs_list, expected {{path, include_hidden?}}: {e}")
            })?;
            Ok(Request::List(ListParams {
                path: require_nonempty_path(p.path, "fs_list")?,
                include_hidden: p.include_hidden,
            }))
        }
        "fs_read" => {
            let p: ReadRaw = serde_json::from_slice(params_json).map_err(|e| {
                format!("ERR_FILES_BAD_PARAMS: invalid params for fs_read, expected {{path, offset?, max_bytes?}}: {e}")
            })?;
            Ok(Request::Read(ReadParams {
                path: require_nonempty_path(p.path, "fs_read")?,
                offset: p.offset,
                max_bytes: p.max_bytes,
            }))
        }
        "fs_write" => {
            let p: WriteRaw = serde_json::from_slice(params_json).map_err(|e| {
                format!("ERR_FILES_BAD_PARAMS: invalid params for fs_write, expected {{path, text?|content_base64?, create_parents?}}: {e}")
            })?;
            let path = require_nonempty_path(p.path, "fs_write")?;
            let bytes = match (p.text, p.content_base64) {
                (Some(t), None) => t.into_bytes(),
                (None, Some(b64)) => base64::engine::general_purpose::STANDARD
                    .decode(b64)
                    .map_err(|e| {
                        format!("ERR_FILES_BAD_PARAMS: `content_base64` is not valid base64: {e}")
                    })?,
                (None, None) => {
                    return Err(
                        "ERR_FILES_BAD_PARAMS: fs_write requires exactly one of `text` or `content_base64`"
                            .to_string(),
                    )
                }
                (Some(_), Some(_)) => {
                    return Err(
                        "ERR_FILES_BAD_PARAMS: fs_write requires exactly one of `text` or `content_base64`, not both"
                            .to_string(),
                    )
                }
            };
            Ok(Request::Write(WriteParams {
                path,
                bytes,
                create_parents: p.create_parents,
            }))
        }
        "fs_delete" => {
            let p: DeleteRaw = serde_json::from_slice(params_json).map_err(|e| {
                format!("ERR_FILES_BAD_PARAMS: invalid params for fs_delete, expected {{path, to_trash?}}: {e}")
            })?;
            Ok(Request::Delete(DeleteParams {
                path: require_nonempty_path(p.path, "fs_delete")?,
                to_trash: p.to_trash.unwrap_or(true),
            }))
        }
        "fs_mkdir" => {
            let p: MkdirRaw = serde_json::from_slice(params_json).map_err(|e| {
                format!("ERR_FILES_BAD_PARAMS: invalid params for fs_mkdir, expected {{path, parents?}}: {e}")
            })?;
            Ok(Request::Mkdir(MkdirParams {
                path: require_nonempty_path(p.path, "fs_mkdir")?,
                parents: p.parents,
            }))
        }
        "fs_rename" => {
            let p: RenameRaw = serde_json::from_slice(params_json).map_err(|e| {
                format!("ERR_FILES_BAD_PARAMS: invalid params for fs_rename, expected {{from, to, overwrite?}}: {e}")
            })?;
            Ok(Request::Rename(RenameParams {
                from: require_nonempty_path(p.from, "fs_rename.from")?,
                to: require_nonempty_path(p.to, "fs_rename.to")?,
                overwrite: p.overwrite,
            }))
        }
        "fs_move" => {
            let p: MoveRaw = serde_json::from_slice(params_json).map_err(|e| {
                format!("ERR_FILES_BAD_PARAMS: invalid params for fs_move, expected {{from, to, overwrite?}}: {e}")
            })?;
            Ok(Request::Move(MoveParams {
                from: require_nonempty_path(p.from, "fs_move.from")?,
                to: require_nonempty_path(p.to, "fs_move.to")?,
                overwrite: p.overwrite,
            }))
        }
        other => Err(format!("ERR_FILES_BAD_PARAMS: unknown action: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_list() {
        match parse_request("fs_list", br#"{"path": "/tmp", "include_hidden": true}"#).unwrap() {
            Request::List(p) => {
                assert_eq!(p.path, "/tmp");
                assert!(p.include_hidden);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn parses_read_defaults() {
        match parse_request("fs_read", br#"{"path": "/a"}"#).unwrap() {
            Request::Read(p) => {
                assert_eq!(p.offset, 0);
                assert_eq!(p.max_bytes, None);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn parses_write_text() {
        match parse_request("fs_write", br#"{"path": "/a", "text": "hi"}"#).unwrap() {
            Request::Write(p) => {
                assert_eq!(p.bytes, b"hi");
                assert!(!p.create_parents);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn write_requires_exactly_one_payload() {
        let err = parse_request("fs_write", br#"{"path": "/a"}"#).unwrap_err();
        assert!(err.contains("exactly one"), "{err}");
        let err = parse_request("fs_write", br#"{"path": "/a", "text": "x", "content_base64": "eA=="}"#)
            .unwrap_err();
        assert!(err.contains("exactly one"), "{err}");
    }

    #[test]
    fn write_rejects_invalid_base64() {
        let err = parse_request("fs_write", br#"{"path": "/a", "content_base64": "!!!not-base64"}"#)
            .unwrap_err();
        assert!(err.contains("content_base64"), "{err}");
    }

    #[test]
    fn rejects_empty_path_and_unknown_action() {
        assert!(parse_request("fs_list", br#"{"path": ""}"#).is_err());
        assert!(parse_request("fs_frobnicate", b"{}").is_err());
    }

    #[test]
    fn parses_delete_with_default_trash() {
        match parse_request("fs_delete", br#"{"path": "/tmp/x"}"#).unwrap() {
            Request::Delete(p) => {
                assert_eq!(p.path, "/tmp/x");
                assert!(p.to_trash);
            }
            other => panic!("wrong variant: {other:?}"),
        }
        match parse_request("fs_delete", br#"{"path": "/tmp/x", "to_trash": false}"#).unwrap() {
            Request::Delete(p) => assert!(!p.to_trash),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn parses_mkdir_with_default_parents() {
        match parse_request("fs_mkdir", br#"{"path": "/tmp/d"}"#).unwrap() {
            Request::Mkdir(p) => {
                assert_eq!(p.path, "/tmp/d");
                assert!(!p.parents);
            }
            other => panic!("wrong variant: {other:?}"),
        }
        match parse_request("fs_mkdir", br#"{"path": "/tmp/d", "parents": true}"#).unwrap() {
            Request::Mkdir(p) => assert!(p.parents),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn parses_rename_with_default_overwrite() {
        match parse_request("fs_rename", br#"{"from": "/a", "to": "/b"}"#).unwrap() {
            Request::Rename(p) => {
                assert_eq!(p.from, "/a");
                assert_eq!(p.to, "/b");
                assert!(!p.overwrite);
            }
            other => panic!("wrong variant: {other:?}"),
        }
        match parse_request(
            "fs_rename",
            br#"{"from": "/a", "to": "/b", "overwrite": true}"#,
        )
        .unwrap()
        {
            Request::Rename(p) => assert!(p.overwrite),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn parses_move_with_default_overwrite() {
        match parse_request("fs_move", br#"{"from": "/a", "to": "/b"}"#).unwrap() {
            Request::Move(p) => {
                assert_eq!(p.from, "/a");
                assert_eq!(p.to, "/b");
                assert!(!p.overwrite);
            }
            other => panic!("wrong variant: {other:?}"),
        }
        match parse_request("fs_move", br#"{"from": "/a", "to": "/b", "overwrite": true}"#)
            .unwrap()
        {
            Request::Move(p) => assert!(p.overwrite),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn rejects_empty_move_fields() {
        assert!(parse_request("fs_move", br#"{"from": "", "to": "/b"}"#).is_err());
        assert!(parse_request("fs_move", br#"{"from": "/a", "to": ""}"#).is_err());
        assert!(parse_request("fs_rename", br#"{"from": "", "to": "/b"}"#).is_err());
        assert!(parse_request("fs_delete", br#"{"path": ""}"#).is_err());
        assert!(parse_request("fs_mkdir", br#"{"path": ""}"#).is_err());
    }
}
