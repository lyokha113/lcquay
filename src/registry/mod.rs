//! Registry HTTP plumbing shared by `OciClient` and providers.

pub mod challenge;
pub mod copy;
pub mod gate;
pub mod http;
pub mod oci;
pub mod paging;

pub use challenge::*;
pub use gate::*;
pub use http::*;
pub use oci::*;
pub use paging::*;
