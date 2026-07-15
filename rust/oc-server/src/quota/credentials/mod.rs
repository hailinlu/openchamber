//! Quota credentials — managed credential file 读写与 normalizer。
//!
//! 对应 Node `quota/credentials/{store,providers}.js`。

pub mod providers;
pub mod store;

// 重新导出 credentials module 的对外 API。
pub use providers::{
    delete_managed_credential, get_managed_credential_status, normalize, normalizers,
    read_managed_credential, write_managed_credential,
};
