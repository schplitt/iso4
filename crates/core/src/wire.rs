//! Bytes the frame layer must agree on with core: the blob probe and format
//! versions, the descriptor token, and the error text cap.

pub use crate::blob::{probe, probe_format_version, read_format_version, V8_BLOB_HEADER_TAG};
pub use crate::errors::{clamp_error_text, MAX_ERROR_TEXT_BYTES};
pub use crate::webcodec::{brand_key_for_token, DESCRIPTOR_TOKEN_LEN};
