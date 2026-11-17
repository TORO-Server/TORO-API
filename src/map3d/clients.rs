//! (実装中)
use std::sync::Arc;
pub struct Clients {}
impl Clients { pub fn new(_grace_minutes: u64) -> Arc<Self> { Arc::new(Self {}) } }
