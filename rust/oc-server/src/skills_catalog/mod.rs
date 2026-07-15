//! Skills catalog — skill discovery, source parsing, git scanning, installation.
//!
//! 对应 Node `packages/web/server/lib/skills-catalog/*` + `feature-routes-runtime.js`
//! 中注注入的 skill 生命周期函数。

pub mod cache;
pub mod git;
pub mod install;
pub mod routes;
pub mod scan;
pub mod skills;
pub mod source;

pub use cache::*;
pub use git::*;
pub use install::*;
pub use scan::*;
pub use skills::*;
pub use source::*;
