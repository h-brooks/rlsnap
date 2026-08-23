//! pgcore: shared foundation for rlsnap and rehearse.
//!
//! Safety invariant: this crate never sends `COMMIT` to Postgres. The only
//! transaction terminator it issues is `ROLLBACK`. (This sentence is the only
//! place the word "COMMIT" appears in this crate's source.)

pub mod catalog;
pub mod config;
pub mod outcome;
pub mod persona;
pub mod sqlsplit;
pub mod tx;
pub mod txguard;

pub use catalog::{Catalog, CatalogDiff};
pub use config::{Config, Mode, Target};
pub use outcome::{classify, Outcome};
pub use persona::{ApplyError, Persona};
pub use tx::{RollbackTx, Savepoint};
