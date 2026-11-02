use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct ApiResponse<T> {
    pub code: i32,
    pub msg: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
}

impl<T> ApiResponse<T> {
    pub fn success(data: T) -> Self {
        Self {
            code: 0,
            msg: "success".to_string(),
            data: Some(data),
        }
    }
}

impl ApiResponse<()> {
    pub fn ok() -> Self {
        Self {
            code: 0,
            msg: "success".to_string(),
            data: None,
        }
    }

    pub fn error(code: i32, msg: impl Into<String>) -> Self {
        Self {
            code,
            msg: msg.into(),
            data: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConnectionDecisionReq {
    pub device_address: Option<String>,
    pub connection_id: String,
}
