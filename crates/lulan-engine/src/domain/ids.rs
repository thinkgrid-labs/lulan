//! Typed identifiers. Newtypes over UUIDv4 so a `TripId` can never be
//! passed where a `RouteId` is expected.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! id_type {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            #[allow(clippy::new_without_default)]
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(f)
            }
        }

        impl From<Uuid> for $name {
            fn from(value: Uuid) -> Self {
                Self(value)
            }
        }
    };
}

id_type!(LocationId);
id_type!(RouteId);
id_type!(TripPatternId);
id_type!(TripId);
id_type!(ResourceId);
id_type!(CapacityUnitId);
id_type!(HoldId);
id_type!(OrderId);
id_type!(TicketId);
