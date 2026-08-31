pub mod client;
pub mod searxng;
mod tool;
mod types;

pub use searxng::{DEFAULT_SEARXNG_URL, default_searxng_url, is_searxng_endpoint};
pub use types::WebSearchConfig;
