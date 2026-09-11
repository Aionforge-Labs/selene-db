//! Database construction configuration.

/// Storage mode supported by the current facade.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub enum OpenMode {
    /// Keep all graph state in process memory.
    #[default]
    InMemory,
    /// Format-2 persistence selected through the fallible create/open entrypoints.
    /// This variant is not accepted by the infallible builder configuration.
    Durable,
}

/// Configuration consumed by [`DatabaseBuilder`](crate::DatabaseBuilder).
///
/// This is the infallible memory-builder configuration, not a persistence path or
/// an open-or-create request. Durable construction uses explicit fallible methods
/// on Database, without accepting an ignored builder storage mode. Inspect an
/// instance's actual mode with [`Database::open_mode`](crate::Database::open_mode).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DatabaseConfig {
    open_mode: OpenMode,
}

impl DatabaseConfig {
    /// Return the mode selected by the infallible memory builder.
    #[must_use]
    pub const fn open_mode(&self) -> OpenMode {
        self.open_mode
    }
}
