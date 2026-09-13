//! Mail: the IMAP/SMTP engine, its cache, and the types they exchange.

pub mod engine;
pub mod imap;
pub mod model;
pub mod parse;
pub mod query;
pub mod smtp;
pub mod store;

pub use engine::{Command, ConnectionState, Engine, Event};
pub use model::{
    Addr, Draft, Envelope, Flags, MailboxInfo, MessageBody, MessageKey, RowKey, SearchScope,
    SpecialUse,
};
pub use query::Query;
pub use store::Store;
