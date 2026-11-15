//! Operation-owned, bounded temporary storage for ordered records and exact string membership.
mod exact;
pub(crate) mod ordered;
pub(crate) use exact::ExactStrings;
pub(crate) use ordered::{read_string, write_string, Ordered, Record};
